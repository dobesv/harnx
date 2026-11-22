//! Native server assembly, HTTP listener and background lifecycle tasks.

use anyhow::{Context, Result};
use harnx_core::instance::ServerScope;
use harnx_nats_common::connect::NatsConnection;
use harnx_toolset_server::RegistrationIdentity;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

use super::oauth::{NativeAuthFactory, OAuthBackend};
use super::pool::{HttpUpstreamConnector, UpstreamPool};
use super::registrations::{Lifecycle, LifecycleInit, LIST_CHANGED_DEBOUNCE};
use super::toolset::RemoteCredentialsToolset;
use super::{connect_http, registrations, NativeConfig, DEFAULT_REFRESH_INTERVAL};

/// One assembled native tool server: the toolset the runner hosts over NATS
/// plus the connect-page HTTP listener and the background lifecycle tasks.
pub struct NativeServer {
    pub toolset: RemoteCredentialsToolset,
    pub lifecycle: Arc<Lifecycle>,
    /// Base URL the connect links use (public base or the bound loopback port).
    pub base_url: String,
    /// Local listener address (for tests; pages bind here regardless of the
    /// public base URL's host).
    pub local_addr: SocketAddr,
}

/// Cancellation and registration timing shared by native background tasks.
pub struct NativeServerRuntime {
    pub shutdown: CancellationToken,
    pub refresh_interval: Duration,
}

impl NativeServer {
    pub async fn start(
        config: NativeConfig,
        connection: NatsConnection,
        scope: ServerScope,
        identity: RegistrationIdentity,
        runtime: NativeServerRuntime,
    ) -> Result<Self> {
        let (lifecycle, list_changed_rx) =
            build_lifecycle(config, connection, scope, identity, runtime).await?;

        // Bind the connect pages before the toolset answers `<server>_connect`,
        // so the link in the very first challenge is already reachable.
        let (base_url, local_addr) = start_connect_http(lifecycle.clone()).await?;

        start_background_tasks(lifecycle.clone(), list_changed_rx);

        let toolset = RemoteCredentialsToolset::new(lifecycle.clone(), base_url.clone());
        Ok(Self {
            toolset,
            lifecycle,
            base_url,
            local_addr,
        })
    }
}

async fn start_connect_http(lifecycle: Arc<Lifecycle>) -> Result<(String, SocketAddr)> {
    let config = &lifecycle.config;
    let listener = tokio::net::TcpListener::bind(config.http_bind)
        .await
        .with_context(|| format!("bind connect pages on {}", config.http_bind))?;
    let local_addr = listener.local_addr()?;
    let base_url = match &config.public_base_url {
        Some(public) => public.to_string(),
        None => format!("http://127.0.0.1:{}", local_addr.port()),
    };
    let app = connect_http::router(lifecycle.clone(), &base_url);
    let shutdown = lifecycle.shutdown.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await
    });
    Ok((base_url, local_addr))
}

/// Native-mode entry: host the toolset over NATS. Connects the NATS
/// transport itself (the shared runner's factory hook is synchronous, but
/// native startup opens JetStream stores and binds the connect pages), then
/// hands the toolset to `serve_with_shutdown`, which publishes and refreshes
/// the global management registration exactly like any other native server.
/// Scope and identity come from the same environment variables the runner
/// reads (`HARNX_SERVER_SCOPE`, `HARNX_SERVER_PACKAGE`, `HARNX_SERVER_CONFIG`).
pub async fn run_native(config: NativeConfig) -> Result<()> {
    let service_name = format!("harnx-{}-server", config.server_name);
    let telemetry = harnx_telemetry::init_telemetry(&service_name)?;

    let result = run_native_inner(config).await;
    telemetry.shutdown().await;
    result
}

async fn run_native_inner(config: NativeConfig) -> Result<()> {
    let metrics_addr = std::env::var("HARNX_METRICS_ADDR").ok();
    harnx_metrics::init(&harnx_metrics::MetricsFlags { metrics_addr })?;
    let healthz_addr = std::env::var("HARNX_HEALTHZ_ADDR").ok();
    let readiness = harnx_healthz::init(&harnx_healthz::HealthzFlags { healthz_addr }).await?;

    let endpoint = harnx_nats_common::connect::NatsEndpoint::from_env()?;
    let client = endpoint.connect().await?;
    let connection = NatsConnection {
        client,
        replicas: endpoint.resolved_replicas(),
    };
    let scope =
        harnx_core::instance::scope_from_env(harnx_core::instance::StandaloneMode::McpStdio)?;
    let identity = RegistrationIdentity::new(
        std::env::var(harnx_toolset::HARNX_SERVER_PACKAGE)
            .ok()
            .filter(|package| !package.is_empty()),
        std::env::var(harnx_toolset::HARNX_SERVER_CONFIG).unwrap_or_default(),
    );
    let shutdown = harnx_nats_common::shutdown::cancel_token_on_shutdown_signal();
    let server = NativeServer::start(
        config,
        connection.clone(),
        scope.clone(),
        identity,
        NativeServerRuntime {
            shutdown: shutdown.clone(),
            refresh_interval: DEFAULT_REFRESH_INTERVAL,
        },
    )
    .await?;
    log::info!("serving native mode under scope '{}'", scope.as_str());
    let result = harnx_toolset_server::serve_with_shutdown(
        Arc::new(server.toolset),
        scope,
        connection,
        harnx_toolset_server::ServeLifecycle::new(shutdown.clone(), readiness),
    )
    .await;
    // Stop the background loops (registration refresh, list_changed, connect
    // pages) once the serve loop has exited.
    shutdown.cancel();
    result
}

type ListChangedReceiver = tokio::sync::mpsc::Receiver<super::pool::ListChanged>;

async fn build_lifecycle(
    config: NativeConfig,
    connection: NatsConnection,
    scope: ServerScope,
    identity: RegistrationIdentity,
    runtime: NativeServerRuntime,
) -> Result<(Arc<Lifecycle>, ListChangedReceiver)> {
    let keys = Arc::new(harnx_credentials::Keyring::load(
        &config.credentials_key_file,
    )?);
    let js = async_nats::jetstream::new(connection.client.clone());
    let replicas = connection.replicas;
    let identity_token = harnx_toolset::server_identity_token(
        identity.package.as_deref(),
        &identity.config,
        &config.server_name,
    );
    let NativeStores {
        credentials,
        flows,
        catalogs,
        leases,
    } = NativeStores::open(&config, &js, replicas, keys, &identity_token).await?;
    let handle = harnx_toolset_server::RegistrationHandle::open(
        &connection.client,
        replicas,
        scope,
        identity.clone(),
        &config.server_name,
    )
    .await?;
    let result_limits = harnx_toolset_server::result_limits::ResultLimits::default()
        .with_storage(js.clone(), replicas);
    let connector = Arc::new(HttpUpstreamConnector::from_config(&config)?);
    let auth_factory = Arc::new(NativeAuthFactory {
        credentials: credentials.clone(),
        leases: leases.clone(),
        catalogs: catalogs.clone(),
        handle: handle.clone(),
        config: config.clone(),
    });
    let oauth = config
        .auth_methods
        .oauth
        .then(|| OAuthBackend::new(config.clone(), flows.clone()).map(Arc::new))
        .transpose()?;
    let (pool, list_changed_rx) = UpstreamPool::new(connector, auth_factory.clone());
    let mut lifecycle = Lifecycle::new(LifecycleInit {
        config: config.clone(),
        identity,
        identity_token,
        credentials,
        flows,
        catalogs,
        leases,
        handle,
        pool,
        auth_factory,
        result_limits,
        refresh_interval: runtime.refresh_interval,
        shutdown: runtime.shutdown,
    });
    lifecycle.oauth = oauth;
    Ok((Arc::new(lifecycle), list_changed_rx))
}

struct NativeStores {
    credentials: harnx_credentials::CredentialStore,
    flows: harnx_credentials::PendingFlowStore,
    catalogs: harnx_credentials::CatalogStore,
    leases: harnx_credentials::RefreshLeaseStore,
}

impl NativeStores {
    async fn open(
        config: &NativeConfig,
        js: &async_nats::jetstream::Context,
        replicas: usize,
        keys: Arc<harnx_credentials::Keyring>,
        identity_token: &str,
    ) -> Result<Self> {
        // Base stores are principal-scoped on use via `for_principal`; the
        // ownerless base keeps every derived store under the same keyring.
        let credentials = harnx_credentials::CredentialStore::open(
            js,
            replicas,
            keys.clone(),
            identity_token,
            harnx_credentials::Principal::Ownerless,
        )
        .await?
        .with_credential_ttl(config.credential_ttl)?;
        let flows =
            harnx_credentials::PendingFlowStore::open(js, replicas, keys.clone(), identity_token)
                .await?;
        let catalogs = harnx_credentials::CatalogStore::open(
            js,
            replicas,
            identity_token,
            harnx_credentials::Principal::Ownerless,
        )
        .await?;
        // Opened now so T6 can plug refresh in without changing startup; the
        // token flow never touches it.
        let leases = harnx_credentials::RefreshLeaseStore::open(js, replicas).await?;
        Ok(Self {
            credentials,
            flows,
            catalogs,
            leases,
        })
    }
}

fn start_background_tasks(lifecycle: Arc<Lifecycle>, list_changed_rx: ListChangedReceiver) {
    // Startup recovery and the registration refresh loop (decision 5).
    let bg = lifecycle.clone();
    tokio::spawn(async move {
        if let Err(error) = bg.recover_at_startup().await {
            log::warn!("startup registration recovery failed: {error:#}");
        }
        let mut ticker = tokio::time::interval(bg.refresh_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // interval's first tick completes immediately
        loop {
            tokio::select! {
                _ = bg.shutdown.cancelled() => break,
                _ = ticker.tick() => {
                    if let Err(error) = bg.refresh_tick().await {
                        log::warn!("user registration refresh failed: {error:#}");
                    }
                }
            }
        }
    });

    // `tools/list_changed` republishing, debounced (decision 9).
    let bg = lifecycle.clone();
    tokio::spawn(async move {
        registrations::run_list_changed_loop(bg, list_changed_rx, LIST_CHANGED_DEBOUNCE).await;
    });
}
