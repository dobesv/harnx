use super::*;
use crate::native::client_handler::PoolClientHandler;
use crate::{
    cli::Cli,
    native::pool::{TokenAuthFactory, UpstreamAuth, UpstreamConnection, UpstreamConnector},
};
use clap::Parser;
use harnx_core::instance::ServerScope;
use harnx_credentials::{
    Catalog, CredentialMaterial, Keyring, NewCredential, Secret, TokenMaterial,
};
use harnx_toolset_server::{RegistrationHandle, StoredRegistration};

pub(super) struct Fixture {
    pub js: async_nats::jetstream::Context,
    pub client: async_nats::Client,
    pub keys: Arc<Keyring>,
    pub config: NativeConfig,
    pub _nats: harnx_test_bins::NatsServerHandle,
}

impl Fixture {
    pub async fn new() -> Self {
        harnx_core::require_nextest();
        let nats = harnx_test_bins::spawn_nats_server()
            .await
            .unwrap()
            .expect("NATS tests require NATS_SERVER_BIN; no silent skips");
        let client = async_nats::connect(nats.url()).await.unwrap();
        let js = async_nats::jetstream::new(client.clone());
        let key_file = Keyring::generate_key_file().unwrap();
        let keys = Arc::new(Keyring::from_json(key_file.expose().as_bytes()).unwrap());
        let cli = Cli::parse_from([
            "harnx-mcp-remote",
            "--url",
            "https://upstream.example/mcp",
            "--name",
            "jira",
            "--auth-methods",
            "token",
            "--credentials-key-file",
            "/unused-test-key.json",
        ]);
        let config = NativeConfig::from_cli(&cli).unwrap();
        Self {
            js,
            client,
            keys,
            config,
            _nats: nats,
        }
    }

    pub async fn lifecycle(&self) -> Lifecycle {
        let identity = RegistrationIdentity {
            package: None,
            config: "test".into(),
        };
        let identity_token = harnx_toolset::server_identity_token(None, &identity.config, "jira");
        let credentials = CredentialStore::open(
            &self.js,
            1,
            self.keys.clone(),
            &identity_token,
            Principal::Ownerless,
        )
        .await
        .unwrap();
        let flows = PendingFlowStore::open(&self.js, 1, self.keys.clone(), &identity_token)
            .await
            .unwrap();
        let catalogs = CatalogStore::open(&self.js, 1, &identity_token, Principal::Ownerless)
            .await
            .unwrap();
        let leases = RefreshLeaseStore::open(&self.js, 1).await.unwrap();
        let handle = RegistrationHandle::open(
            &self.client,
            1,
            ServerScope::from_string("registrations-test"),
            identity.clone(),
            "jira",
        )
        .await
        .unwrap();
        let auth_factory = Arc::new(TokenAuthFactory {
            header: self.config.token_header.clone(),
            scheme: self.config.token_scheme.clone(),
        });
        let connector = Arc::new(NoUpstream);
        let (pool, _) = UpstreamPool::new(connector, auth_factory.clone());
        Lifecycle::new(LifecycleInit {
            config: self.config.clone(),
            identity,
            identity_token,
            credentials,
            flows,
            catalogs,
            leases,
            handle,
            pool,
            auth_factory,
            result_limits: ResultLimits::default(),
            refresh_interval: Duration::from_secs(30),
            shutdown: CancellationToken::new(),
        })
    }

    /// This handle's bucket is deliberately unavailable; restoring the real handle
    /// lets the same lifecycle retry after a genuine JetStream read error.
    pub async fn broken_registration_handle(&self, lifecycle: &Lifecycle) -> RegistrationHandle {
        let kv = self
            .js
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: "unavailable_registry".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        self.js
            .delete_key_value("unavailable_registry")
            .await
            .unwrap();
        RegistrationHandle::new(
            kv,
            ServerScope::from_string("registrations-test"),
            lifecycle.identity_token.clone(),
        )
    }
}

pub(super) fn alice() -> Principal {
    Principal::User("alice".into())
}

pub(super) fn tools(name: &str) -> Vec<rmcp::model::Tool> {
    vec![rmcp::model::Tool::new(
        name.to_owned(),
        name.to_owned(),
        Arc::new(serde_json::Map::new()),
    )]
}

pub(super) async fn connect(lifecycle: &Lifecycle, principal: &Principal, name: &str) -> String {
    let input = NewCredential {
        material: CredentialMaterial::Token(TokenMaterial {
            token: Secret::new("test-secret".into()),
            header: "authorization".into(),
            scheme: Some("Bearer".into()),
        }),
        account_label: None,
        expires_at: None,
    };
    let credential = lifecycle
        .connect_credential(principal, input)
        .await
        .unwrap();
    let connection_id = credential.record.connection_id;
    let catalogs = lifecycle.catalogs.for_principal(principal.clone()).unwrap();
    let credentials = lifecycle
        .credentials
        .for_principal(principal.clone())
        .unwrap();
    let catalog = Catalog {
        connection_id: connection_id.clone(),
        tools: tools(name),
    };
    catalogs
        .publish(
            &credentials,
            catalogs.inspect().await.unwrap().revision,
            catalog,
        )
        .await
        .unwrap();
    lifecycle
        .publish_registration(principal, &connection_id, &tools(name), true)
        .await
        .unwrap();
    connection_id
}

pub(super) async fn registration(
    lifecycle: &Lifecycle,
    principal: &Principal,
) -> StoredRegistration {
    lifecycle
        .handle
        .read(&Lifecycle::audience(principal))
        .await
        .unwrap()
        .unwrap()
}

pub(super) async fn assert_refreshes(
    lifecycle: &Lifecycle,
    principal: &Principal,
    connection_id: &str,
) {
    let before = registration(lifecycle, principal).await;
    lifecycle.refresh_tick().await.unwrap();
    let after = registration(lifecycle, principal).await;
    assert!(
        after.revision > before.revision,
        "still refreshed without restart"
    );
    assert_eq!(
        after.registration.connection_id.as_deref(),
        Some(connection_id)
    );
}

struct NoUpstream;

#[async_trait::async_trait]
impl UpstreamConnector for NoUpstream {
    async fn connect(
        &self,
        _: Arc<dyn UpstreamAuth>,
        _: PoolClientHandler,
    ) -> Result<UpstreamConnection> {
        panic!("registration lifecycle must not contact the upstream");
    }
}
