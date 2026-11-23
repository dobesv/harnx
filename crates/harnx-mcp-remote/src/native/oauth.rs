//! Durable OAuth acquisition and lease-fenced refresh. rmcp owns protocol logic;
//! encrypted stores own all state. Provider diagnostics never leave this module.

use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use harnx_credentials::{
    flow::OAuthContext, rmcp_adapters::RmcpStateStore, ClientRegistration, CredentialMaterial,
    FlowOptions, NewCredential, PendingFlow, PendingFlowStore, Secret, Versioned,
};
use oauth2::TokenResponse;
use rmcp::transport::auth::{
    AuthorizationManager, AuthorizationMetadata, ClientRegistrationResponse, OAuthClientConfig,
};
use tracing::instrument::WithSubscriber;
use url::Url;

use super::{connect_http::OAuthFlowBackend, oauth_http::OAuthHttp, NativeConfig};

mod client;
mod refresh;

pub(crate) use client::{client_config_from, pin_auth_method, restored_manager, CaptureState};
pub use refresh::NativeAuthFactory;

const DCR_ERROR: &str = "OAuth dynamic client registration failed; configure --oauth-client-id (and --oauth-client-secret-file if required) or use --auth-methods token";
pub(crate) const OAUTH_ERROR: &str =
    "OAuth provider request failed; check the server's OAuth configuration or try connecting again";

pub struct OAuthBackend {
    config: NativeConfig,
    client_secret: Option<Secret<String>>,
    flows: PendingFlowStore,
}

impl OAuthBackend {
    pub fn new(config: NativeConfig, flows: PendingFlowStore) -> Result<Self> {
        let client_secret = read_client_secret(&config)?;
        OAuthHttp::new(&config)?;
        Ok(Self {
            config,
            client_secret,
            flows,
        })
    }

    /// All protocol state, including PKCE and discovery, is saved before the
    /// model receives a connect challenge. No process-local OAuth session remains.
    pub async fn prepare(&self, redirect_uri: String) -> Result<FlowOptions> {
        self.prepare_inner(redirect_uri)
            .with_subscriber(tracing::subscriber::NoSubscriber::default())
            .await
    }

    async fn prepare_inner(&self, redirect_uri: String) -> Result<FlowOptions> {
        let http = Arc::new(OAuthHttp::new(&self.config)?);
        let (mut manager, mut metadata) =
            discover_server_metadata(&self.config, http.clone()).await?;

        let scopes: Vec<&str> = self
            .config
            .oauth_scopes
            .iter()
            .map(String::as_str)
            .collect();

        let (client_config, registration) = self
            .configure_client(&mut manager, &redirect_uri, &scopes, &http)
            .await?;

        let client =
            build_client_registration(client_config, registration, &metadata, redirect_uri.clone());

        pin_auth_method(&mut metadata, &client)?;
        manager.set_metadata(metadata.clone());
        manager
            .configure_client(client_config_from(&client, &self.config.oauth_scopes))
            .map_err(|_| anyhow!(OAUTH_ERROR))?;

        let capture = CaptureState::default();
        manager.set_state_store(capture.clone());

        let authorization_url = manager
            .get_authorization_url(&scopes)
            .await
            .map_err(|_| anyhow!(OAUTH_ERROR))?;
        let url = Url::parse(&authorization_url).map_err(|_| anyhow!(OAUTH_ERROR))?;
        http.validate_url(&url)
            .await
            .map_err(|_| anyhow!(OAUTH_ERROR))?;

        let resource = extract_resource_param(&url, &self.config.url);
        let state = capture.take_state()?;

        Ok(FlowOptions {
            redirect_uri,
            scopes: state.requested_scopes.clone(),
            oauth_client: Some(client),
            oauth_context: Some(OAuthContext {
                metadata: Secret::new(metadata),
                resource,
                authorization_url: Secret::new(authorization_url),
            }),
            authorization_state: Some(Secret::new(state)),
        })
    }

    async fn configure_client(
        &self,
        manager: &mut AuthorizationManager,
        redirect_uri: &str,
        scopes: &[&str],
        http: &OAuthHttp,
    ) -> Result<(
        OAuthClientConfig,
        Option<Secret<ClientRegistrationResponse>>,
    )> {
        if let Some(client_id) = &self.config.oauth_client_id {
            let mut client = OAuthClientConfig::new(client_id, redirect_uri)
                .with_scopes(self.config.oauth_scopes.clone());
            if let Some(secret) = &self.client_secret {
                client = client.with_client_secret(secret.expose());
            }
            manager
                .configure_client(client.clone())
                .map_err(|_| anyhow!(OAUTH_ERROR))?;
            Ok((client, None))
        } else {
            let client = manager
                .register_client(
                    &format!("harnx {}", self.config.server_name),
                    redirect_uri,
                    scopes,
                )
                .await
                .map_err(|_| anyhow!(DCR_ERROR))?;
            let registration = http.take_registration().ok_or_else(|| anyhow!(DCR_ERROR))?;
            Ok((client, Some(registration)))
        }
    }
}

fn read_client_secret(config: &NativeConfig) -> Result<Option<Secret<String>>> {
    if config.oauth_client_secret_file.is_some() && config.oauth_client_id.is_none() {
        bail!("--oauth-client-secret-file requires --oauth-client-id");
    }
    let Some(path) = &config.oauth_client_secret_file else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path)
        .map_err(|_| anyhow!("cannot read --oauth-client-secret-file"))?;
    let text = text.trim_end_matches(['\r', '\n']);
    anyhow::ensure!(!text.is_empty(), "--oauth-client-secret-file is empty");
    Ok(Some(Secret::from(text)))
}

async fn discover_server_metadata(
    config: &NativeConfig,
    http: Arc<OAuthHttp>,
) -> Result<(AuthorizationManager, AuthorizationMetadata)> {
    let mut manager = AuthorizationManager::new_with_oauth_http_client(&config.url, http)
        .await
        .map_err(|_| anyhow!(OAUTH_ERROR))?;
    let resolution = manager
        .resolve_metadata()
        .await
        .map_err(|_| anyhow!(OAUTH_ERROR))?;
    let metadata = resolution.metadata;
    anyhow::ensure!(
        metadata.issuer.is_some(),
        "OAuth metadata must include an issuer"
    );
    manager.set_metadata(metadata.clone());
    Ok((manager, metadata))
}

fn build_client_registration(
    client_config: OAuthClientConfig,
    registration: Option<Secret<ClientRegistrationResponse>>,
    metadata: &AuthorizationMetadata,
    redirect_uri: String,
) -> ClientRegistration {
    let auth_method = registration
        .as_ref()
        .and_then(|response| {
            response
                .expose()
                .additional_fields
                .get("token_endpoint_auth_method")
                .and_then(serde_json::Value::as_str)
        })
        .map(str::to_owned)
        .unwrap_or_else(|| {
            client::selected_auth_method(metadata, client_config.client_secret.is_some())
        });

    ClientRegistration {
        client_id: client_config.client_id,
        client_secret: client_config.client_secret.map(Secret::new),
        token_endpoint_auth_method: auth_method,
        redirect_uri,
        registration,
    }
}

fn extract_resource_param(url: &Url, default_resource: &str) -> String {
    url.query_pairs()
        .find(|(key, _)| key == "resource")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| default_resource.to_string())
}

#[async_trait]
impl OAuthFlowBackend for OAuthBackend {
    async fn redirect_url(&self, flow: &Versioned<PendingFlow>) -> Result<Url, String> {
        let context = flow.record.oauth_context.as_ref().ok_or(OAUTH_ERROR)?;
        let url =
            Url::parse(context.authorization_url.expose()).map_err(|_| OAUTH_ERROR.to_owned())?;
        OAuthHttp::new(&self.config)
            .map_err(|_| OAUTH_ERROR.to_owned())?
            .validate_url(&url)
            .await
            .map_err(|_| OAUTH_ERROR.to_owned())?;
        Ok(url)
    }

    async fn exchange_code(
        &self,
        flow: &Versioned<PendingFlow>,
        code: &str,
    ) -> Result<NewCredential, String> {
        self.exchange_code_with_issuer(flow, code, None).await
    }

    async fn exchange_code_with_issuer(
        &self,
        flow: &Versioned<PendingFlow>,
        code: &str,
        issuer: Option<&str>,
    ) -> Result<NewCredential, String> {
        async {
            let context = flow.record.oauth_context.as_ref().ok_or(OAUTH_ERROR)?;
            let client = flow.record.oauth_client.as_ref().ok_or(OAUTH_ERROR)?;
            let http = Arc::new(OAuthHttp::new(&self.config).map_err(|_| OAUTH_ERROR)?);
            let mut manager = restored_manager(
                &context.resource,
                context.metadata.expose().clone(),
                client,
                &flow.record.scopes,
                http.clone(),
            )
            .await
            .map_err(|_| OAUTH_ERROR)?;
            manager.set_state_store(RmcpStateStore::new(
                self.flows.clone(),
                flow.record.challenge_id.clone(),
            ));
            let token = manager
                .exchange_code_for_token_with_issuer(code, flow.record.oauth_state.expose(), issuer)
                .await
                .map_err(|_| "OAuth code exchange failed; start a new connection")?;
            let raw_label = http
                .account_label(context.metadata.expose(), token.access_token().secret())
                .await
                .or_else(|| {
                    token
                        .extra_fields()
                        .0
                        .get("email")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                });
            let account_label =
                client::sanitize_account_label(raw_label, code, &flow.record, &token, client);
            let material =
                client::build_oauth_material(context, client, &token, &flow.record.scopes)
                    .map_err(|_| OAUTH_ERROR)?;
            Ok(NewCredential {
                material: CredentialMaterial::Oauth2(Box::new(material)),
                account_label,
                expires_at: None,
            })
        }
        .with_subscriber(tracing::subscriber::NoSubscriber::default())
        .await
        .map_err(str::to_owned)
    }
}
