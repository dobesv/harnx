//! harnx-serve — HTTP server front-end for the harnx agent harness
//! (plan P47, β+ progressive peel). Extracted from `harnx::serve`.
//! Depends on `harnx-runtime` for Config + Client orchestration.

pub mod ag_ui;
mod ag_ui_attach;
mod ag_ui_events;
mod ag_ui_interactions;
mod ag_ui_lifecycle;
mod ag_ui_remote_follow;
pub mod ag_ui_rpc;
mod ag_ui_surfaces;
mod ag_ui_sync;
mod ag_ui_usage;
mod agent_access;
mod agent_resolve;
mod attachments;
mod cid;
mod history_projection;
mod interrupt_resume;
mod last_turn;
mod models_catalog;
mod nats_access;
mod nats_redaction;
mod parking_gate;
mod request_identity;
mod serve_shutdown;
mod session_access;
pub mod session_actor;
mod session_actor_types;
mod session_claim;
mod session_claim_reservation;
mod session_history;
pub(crate) mod session_pagination;
pub mod session_routes;
mod stalled_turn;
mod transcript_snapshot;
mod upload_storage;
mod web_url;

pub use serve_shutdown::{ShutdownConfig, StreamDrainConfig};

// Not `#[cfg(test)]`: the `tests/` integration crates link the library built
// WITHOUT the `test` cfg, so gating this out would break their
// `harnx_serve::test_support` imports. Kept public for cross-crate test reuse.
pub mod test_support;

#[cfg(test)]
mod access_tests;
#[cfg(test)]
mod cid_route_tests;
#[cfg(test)]
mod nats_access_tests;
#[cfg(test)]
mod nats_session_claim_tests;
#[cfg(test)]
mod nats_user_identity_tests;
#[cfg(test)]
mod nats_web_session_url_tests;
#[cfg(test)]
mod remote_agent_nats_tests;

use request_identity::RequestIdentity;
use session_access::list_target_sessions;

pub(crate) use agent_resolve::{is_safe_agent_path, is_safe_path_segment, resolve_agent_target};
pub(crate) use nats_access::{
    nats_cluster_unavailable, sanitize_nats_cluster_error, sanitize_nats_session_error,
    serve_nats_client, serve_nats_jetstream,
};

use crate::ag_ui::AgUiError;
use crate::ag_ui_rpc::{handle_ag_ui_rpc, PersistenceKind, RpcContext};
use crate::session_actor::{ResolvedAgentTarget, SessionRegistry};
use crate::session_routes::{AgentSessionRef, SessionsRouteContext};
use attachments::*;

use harnx_core::access_rules::{AccessRules, CallerView};
#[cfg(test)]
use harnx_core::agent_ref::AgentRef;
use harnx_core::message::MessageRole;
use harnx_core::session::SessionLogEntry;
use harnx_core::user_aliases::UserAliases;
use harnx_rag::*;
use harnx_runtime::{client::*, config::*, utils::*};
use log::{debug, error, info, warn};

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::stream::StreamExt;
use harnx_blob_store::media::AttachmentsBucket;
#[cfg(test)]
use harnx_core::attachments::store_attachment_bytes_async;
use harnx_core::attachments::{collect_cid_refs, CID_PREFIX};
use harnx_core::cid_url::{CidUrl, SessionRef};
use harnx_runtime::config::ConfigLock;
use http::{Method, Response, StatusCode};
use http_body::Body;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::graceful::GracefulShutdown,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    convert::Infallible,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock},
    time::{Duration, Instant, SystemTime},
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinSet};

use models_catalog::advertised_models;

/// Default ceiling for closing client-facing HTTP connections during shutdown.
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(25);

static LOCAL_NATS_HANDLES: LazyLock<
    tokio::sync::Mutex<HashMap<PathBuf, harnx_runtime::nats_local_server::SharedNatsServer>>,
> = LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// Maximum upload size in bytes (20 MiB).
/// Enforced during streaming to prevent OOM from oversized payloads.
pub const MAX_UPLOAD_BYTES: usize = 20 * 1024 * 1024;

type AppResponse = Response<BoxBody<Bytes, Infallible>>;

/// Log a redacted snapshot of the process environment relevant to credential
/// and config-path resolution for child processes.
///
/// harnx-serve does not clear or filter the inherited process environment
/// processes — they inherit it exactly like the TUI does. When sub-agents fail
/// with "missing credentials" only under harnx-serve, the usual cause is that
/// the *service* environment (systemd unit, container, launcher) lacks the
/// user-session context the TUI runs with: a different or missing `HOME` /
/// `XDG_DATA_HOME` / `HARNX_DATA_DIR` (so `~/.local/share/harnx/.env` never
/// resolves), no inherited `*_API_KEY` vars, or no `DBUS_SESSION_BUS_ADDRESS` /
/// `XDG_RUNTIME_DIR` (so keyring/`secret-tool` lookups fail).
///
/// This never logs secret values — only presence/absence and, for API keys,
/// the variable *names*. See `docs/harnx-serve-subagent-credentials.md`.
fn log_startup_environment_diagnostics() {
    fn present(var: &str) -> &'static str {
        match std::env::var_os(var) {
            Some(value) if !value.is_empty() => "present",
            Some(_) => "present-but-empty",
            None => "MISSING",
        }
    }

    info!(
        "harnx-serve environment diagnostics (redacted; for sub-agent credential troubleshooting):"
    );
    for var in [
        "HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
        "XDG_STATE_HOME",
        "DBUS_SESSION_BUS_ADDRESS",
        "HARNX_DATA_DIR",
        "HARNX_STATE_DIR",
        "HARNX_ENV_FILE",
        "PATH",
    ] {
        info!("  env {var}: {}", present(var));
    }

    // The `.env` file the runtime loads at startup; child processes resolve the
    // same path, so a missing file here means neither inherits its credentials.
    let env_file = harnx_core::config_paths::env_file();
    let env_file_status = if env_file.is_file() {
        "found"
    } else {
        "NOT FOUND"
    };
    info!(
        "  resolved .env file: {} ({env_file_status})",
        env_file.display()
    );
    info!(
        "  resolved data dir: {}",
        harnx_core::config_paths::data_dir().display()
    );

    // Names only (never values) of credential-looking vars visible to child
    // agents. Matches common suffixes across providers so troubleshooting isn't
    // limited to the `_API_KEY` convention (e.g. AWS_SECRET_ACCESS_KEY, GH_TOKEN,
    // ANTHROPIC_AUTH_TOKEN). Value bytes are never read or logged.
    let mut credential_names: Vec<String> = std::env::vars_os()
        .filter_map(|(key, _)| key.into_string().ok())
        .filter(|key| {
            key.ends_with("_API_KEY")
                || key.ends_with("_TOKEN")
                || key.ends_with("_SECRET")
                || key.ends_with("_ACCESS_KEY")
                || key.ends_with("_KEY")
        })
        .collect();
    credential_names.sort();
    if credential_names.is_empty() {
        info!(
            "  credential-like env vars (*_API_KEY/_TOKEN/_SECRET/_KEY): none visible \
             in process env (sub-agents relying on env-var credentials will fail \
             unless creds live in the .env file or a reachable keyring)"
        );
    } else {
        info!(
            "  credential-like env vars visible ({}, names only): {}",
            credential_names.len(),
            credential_names.join(", ")
        );
    }
}

/// Build a browser-openable base URL (`http://host:port`) from a bound socket
/// address. Wildcard bind hosts (`0.0.0.0`, `::`) aren't directly reachable in
/// a browser, so map them to loopback. IPv6 hosts are bracketed per RFC 3986,
/// and a link-local scope/zone id is preserved with the RFC 6874 `%25` escape.
fn browser_base_url(addr: SocketAddr) -> String {
    // A zone/scope id only exists on IPv6 sockets; carry it through so a
    // link-local bind (e.g. `fe80::1%eth0`) yields a usable URL.
    let scope_id = match addr {
        SocketAddr::V6(v6) => v6.scope_id(),
        SocketAddr::V4(_) => 0,
    };
    // Normalize IPv4-mapped IPv6 (e.g. `::ffff:0.0.0.0`) to plain IPv4 so
    // wildcard detection and formatting match native IPv4.
    let ip = match addr.ip() {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    };
    let host = match ip {
        IpAddr::V4(v4) if v4.is_unspecified() => Ipv4Addr::LOCALHOST.to_string(),
        IpAddr::V4(v4) => v4.to_string(),
        // Wildcard `::` maps to loopback, which is never scoped.
        IpAddr::V6(v6) if v6.is_unspecified() => format!("[{}]", Ipv6Addr::LOCALHOST),
        IpAddr::V6(v6) if scope_id != 0 => format!("[{v6}%25{scope_id}]"),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    format!("http://{host}:{}", addr.port())
}

/// Choose the base URL to advertise on startup. Prefer the socket's real bound
/// address so ephemeral (`:0`) ports and wildcard hosts resolve correctly; fall
/// back to the requested address string when the OS can't report a local
/// address.
fn advertised_base_url(local_addr: Option<SocketAddr>, requested_addr: &str) -> String {
    match local_addr {
        Some(addr) => browser_base_url(addr),
        None => format!("http://{requested_addr}"),
    }
}

/// Return an actionable warning when the resolved web-assets directory can't
/// serve the UI (missing, or present without an `index.html`), else `None`.
/// Checked once at startup so a container/service with an unmounted or empty
/// assets dir logs why the UI 404s instead of failing silently.
fn web_assets_warning(dir: &Path) -> Option<String> {
    if dir.join("index.html").is_file() {
        None
    } else {
        Some(format!(
            "Web assets unavailable at '{}': directory is missing or does not contain index.html; set HARNX_WEB_ASSETS, pass --web-assets, or run `cargo xtask install`",
            dir.display()
        ))
    }
}

/// Resolve the web-assets directory (explicit path, else the default under the
/// data dir) and log the startup warning once if it can't serve the UI. Kept
/// out of `run()` so the resolve-and-warn branch doesn't inflate its complexity.
fn resolve_web_assets(web_assets: Option<PathBuf>) -> PathBuf {
    let dir = web_assets.unwrap_or_else(|| harnx_core::config_paths::data_dir().join("web-assets"));
    if let Some(warning) = web_assets_warning(&dir) {
        warn!("{warning}");
    }
    dir
}

pub async fn run(
    config: GlobalConfig,
    addr: Option<String>,
    web_assets: Option<PathBuf>,
    readiness: Option<harnx_healthz::Readiness>,
) -> Result<()> {
    run_with_drain_timeout(config, addr, web_assets, readiness, DEFAULT_DRAIN_TIMEOUT).await
}

pub async fn run_with_drain_timeout(
    config: GlobalConfig,
    addr: Option<String>,
    web_assets: Option<PathBuf>,
    readiness: Option<harnx_healthz::Readiness>,
    drain_timeout: Duration,
) -> Result<()> {
    let access_rules = harnx_runtime::access::load_access_rules(None)?;
    run_with_shutdown_config(
        config,
        addr,
        web_assets,
        readiness,
        ShutdownConfig {
            drain_timeout,
            ..ShutdownConfig::default()
        },
        access_rules,
    )
    .await
}

pub async fn run_with_shutdown_config(
    config: GlobalConfig,
    addr: Option<String>,
    web_assets: Option<PathBuf>,
    readiness: Option<harnx_healthz::Readiness>,
    shutdown: ShutdownConfig,
    access_rules: Option<Arc<AccessRules>>,
) -> Result<()> {
    let ShutdownConfig {
        drain_timeout,
        stream_drain,
    } = shutdown;
    log_startup_environment_diagnostics();

    let addr = match addr {
        Some(addr) => {
            if let Ok(port) = addr.parse::<u16>() {
                format!("127.0.0.1:{port}")
            } else if let Ok(ip) = addr.parse::<IpAddr>() {
                format!("{ip}:8000")
            } else {
                addr
            }
        }
        None => config.read().serve_addr(),
    };
    web_url::public_url(&config.read())?;
    let web_assets = resolve_web_assets(web_assets);
    let server = Arc::new(Server::build(
        &config,
        web_assets,
        stream_drain,
        access_rules,
    )?);
    let listener = TcpListener::bind(&addr).await?;
    // Advertise the bound address so URLs reflect the real host/port even when
    // the request used an ephemeral port (":0") or a bare port. Fall back to
    // the requested `addr` string if the OS can't report a local address.
    let local_addr = match listener.local_addr() {
        Ok(local_addr) => Some(local_addr),
        Err(err) => {
            warn!("could not resolve bound listener address, advertising requested address: {err}");
            None
        }
    };
    let base_url = advertised_base_url(local_addr, &addr);
    if let Some(r) = &readiness {
        r.ready();
    }
    let server_handle = server.run(listener, drain_timeout).await?;
    // Lead with the Web UI URL — that's the one you open in a browser. The
    // API endpoints below are POST-only, so they're listed for reference only.
    println!("Web UI:                {base_url}/");
    println!("Embeddings API (POST): {base_url}/v1/embeddings");
    println!("Rerank API (POST):     {base_url}/v1/rerank");
    harnx_nats_common::shutdown::shutdown_signal().await;
    if let Some(r) = &readiness {
        r.not_ready();
    }
    server_handle.shutdown().await
}

#[doc(hidden)]
pub struct Server {
    config: Config,
    identity_sources: harnx_runtime::identity::IdentitySources,
    group_headers: harnx_runtime::identity::MembershipHeaders,
    role_headers: harnx_runtime::identity::MembershipHeaders,
    access_rules: Option<Arc<AccessRules>>,
    user_aliases: Option<Arc<UserAliases>>,
    models: Vec<Value>,
    agents: Vec<AgentConfig>,
    rags: Vec<String>,
    #[allow(dead_code)]
    session_registry: SessionRegistry,
    /// Root directory for web-ui static assets served over HTTP.
    web_assets: PathBuf,
    /// Per-resolved-cluster upload transport cache: one shared JetStream
    /// context plus a resettable bucket-ensured memo per cluster, reused by
    /// every upload. See [`upload_storage`].
    upload_storage: upload_storage::UploadStorage,
    /// F6 test seam: parks an acknowledged upload part between its put and
    /// the session incarnation recheck when a test arms it. Unarmed it is
    /// a no-op; production never arms it.
    upload_recheck_gate: parking_gate::ParkingGate,
    /// T2 test seam: parks a deferred session claim between the guard's
    /// missing-owner read and the leader reread and reservation when a test
    /// arms it, so creation races can be forced deterministically. Unarmed
    /// it is a no-op; production never arms it.
    session_claim_gate: parking_gate::ParkingGate,
    /// How long a cached bucket serves before the next upload re-ensures it,
    /// bounding the retry rate of a failed best-effort replica raise.
    upload_bucket_recheck: Duration,
    shutdown: serve_shutdown::ServeShutdown,
}

impl Server {
    /// Returns `Some(cluster)` when in cluster-client mode, `None` otherwise.
    ///
    /// Delegates to `Config::default_cluster_for_display` for consistency.
    fn default_cluster_for_display(&self) -> Option<&str> {
        self.config.default_cluster_for_display()
    }

    /// Agent reference as shown to clients: bare for the default cluster.
    fn display_ref(&self, target: &ResolvedAgentTarget) -> String {
        target.display_ref_with_default_cluster(self.default_cluster_for_display())
    }
}

/// Escape a request-derived agent or session value for an HTML **text
/// node**. That is all this helper guarantees — it is not sufficient for
/// URLs, JavaScript, CSS or unquoted attributes. `is_safe_path_segment`
/// accepts markup: it rejects only empty values, non-`Normal` path
/// components, and `/` or `\` (leading/trailing whitespace is rejected
/// separately, by `claimable_session_id`, which also enforces the
/// runtime's ID alphabet for deferred claims). The page still renders
/// request-derived values whose provenance has no such contract — rules-
/// off session IDs and agent names can be arbitrary markup — so an
/// unescaped sink is stored XSS against any admin who opens the page
/// (#2855).
fn escape_html_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        escaped.push_str(match character {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '"' => "&quot;",
            '\'' => "&#39;",
            _ => {
                escaped.push(character);
                continue;
            }
        });
    }
    escaped
}

type RouteMatch = (String, Option<String>, AgentsRoute);

struct ServerHandle {
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
    shutdown: serve_shutdown::ServeShutdown,
    drain_timeout: Duration,
}

impl ServerHandle {
    async fn shutdown(self) -> Result<()> {
        self.shutdown.begin(self.drain_timeout);
        let _ = self.stop.send(());
        self.task
            .await
            .context("HTTP server task failed during shutdown")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentsRoute {
    Agent,
    Sessions,
    Session,
    SessionEvents,
    SessionHistory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentsRepresentation {
    Html,
    Json,
    AgUiSse,
    AgUiRpc,
}

impl Server {
    #[doc(hidden)]
    pub fn new(config: &GlobalConfig, web_assets: PathBuf) -> Self {
        Self::new_with_stream_drain(config, web_assets, StreamDrainConfig::default())
            .expect("valid serve configuration")
    }

    #[doc(hidden)]
    pub fn new_with_access_rules(
        config: &GlobalConfig,
        web_assets: PathBuf,
        access_rules: Option<Arc<AccessRules>>,
    ) -> Result<Self> {
        Self::build(
            config,
            web_assets,
            StreamDrainConfig::default(),
            access_rules,
        )
    }

    fn new_with_stream_drain(
        config: &GlobalConfig,
        web_assets: PathBuf,
        stream_drain: StreamDrainConfig,
    ) -> Result<Self> {
        Self::build(config, web_assets, stream_drain, None)
    }

    fn build(
        config: &GlobalConfig,
        web_assets: PathBuf,
        stream_drain: StreamDrainConfig,
        access_rules: Option<Arc<AccessRules>>,
    ) -> Result<Self> {
        let config = config.read().clone();
        let identity_sources =
            harnx_runtime::identity::IdentitySources::new(&config.serve_user_id_sources)
                .context("invalid serve_user_id_sources")?;
        if access_rules.is_some() && identity_sources.sources().is_empty() {
            bail!("access rules require request identity sources; configure --user-id-source, HARNX_SERVE_USER_ID_SOURCES, or serve_user_id_sources");
        }
        let user_aliases =
            harnx_runtime::users::load_user_aliases().context("failed to load user aliases")?;
        let models = advertised_models(&config);
        let session_registry = SessionRegistry::new(config.clone());
        let agents = config.all_agents();
        Ok(Self {
            identity_sources,
            group_headers: harnx_runtime::identity::MembershipHeaders::new(
                &config.serve_group_headers,
            )
            .context("invalid serve_group_headers")?,
            role_headers: harnx_runtime::identity::MembershipHeaders::new(
                &config.serve_role_headers,
            )
            .context("invalid serve_role_headers")?,
            config,
            access_rules,
            user_aliases,
            models,
            agents,
            rags: Config::list_rags(),
            session_registry,
            web_assets,
            upload_storage: upload_storage::UploadStorage::default(),
            upload_recheck_gate: parking_gate::ParkingGate::default(),
            session_claim_gate: parking_gate::ParkingGate::default(),
            upload_bucket_recheck: upload_storage::DEFAULT_BUCKET_RECHECK_INTERVAL,
            shutdown: serve_shutdown::ServeShutdown::new(stream_drain),
        })
    }

    #[doc(hidden)]
    pub async fn list_sessions_json(&self, agent_ref: &str) -> Result<Value> {
        let (target, _) = resolve_agent_target(&self.config, agent_ref)
            .await
            .map_err(ag_ui_error_to_anyhow)?;
        Ok(Value::Array(
            agent_sessions_json(
                &self.config,
                &target,
                self.access_rules
                    .as_deref()
                    .map(|rules| (rules, CallerView::default())),
            )
            .await?,
        ))
    }

    #[doc(hidden)]
    pub async fn list_session_history(&self, agent_ref: &str, session: &str) -> Result<Value> {
        use http_body_util::BodyExt;
        let (target, _) = resolve_agent_target(&self.config, agent_ref)
            .await
            .map_err(ag_ui_error_to_anyhow)?;
        let resp = self.session_history_json(&target, session).await?;
        let body = resp.into_body().collect().await?.to_bytes();
        Ok(serde_json::from_slice(&body)?)
    }

    async fn run(
        self: Arc<Self>,
        listener: TcpListener,
        drain_timeout: Duration,
    ) -> Result<ServerHandle> {
        let (stop, mut stop_rx) = oneshot::channel();
        let shutdown = self.shutdown.clone();
        let task = tokio::spawn(async move {
            let graceful = GracefulShutdown::new();
            let mut connections = JoinSet::new();

            loop {
                tokio::select! {
                    biased;
                    _ = &mut stop_rx => {
                        break;
                    }
                    res = listener.accept() => {
                        let Ok((cnx, _)) = res else {
                            continue;
                        };

                        let stream = TokioIo::new(cnx);
                        let server = self.clone();
                        let watcher = graceful.watcher();
                        connections.spawn(async move {
                            let hyper_service = service_fn(move |request: hyper::Request<Incoming>| {
                                server.clone().handle(request)
                            });
                            let builder = hyper_util::server::conn::auto::Builder::new(
                                TokioExecutor::new(),
                            );
                            let connection =
                                builder.serve_connection_with_upgrades(stream, hyper_service);
                            let _ = watcher.watch(connection.into_owned()).await;
                        });
                    }
                }
            }

            let connection_count = graceful.count();
            if tokio::time::timeout(drain_timeout, graceful.shutdown())
                .await
                .is_err()
            {
                warn!(
                    "HTTP drain reached its {:.3}s deadline with connection(s) still open; closing them",
                    drain_timeout.as_secs_f64(),
                );
                connections.abort_all();
            } else if connection_count > 0 {
                info!("drained {connection_count} HTTP connection(s)");
            }

            while connections.join_next().await.is_some() {}
        });
        Ok(ServerHandle {
            stop,
            task,
            shutdown,
            drain_timeout,
        })
    }

    async fn handle(
        self: Arc<Self>,
        mut req: hyper::Request<Incoming>,
    ) -> std::result::Result<AppResponse, hyper::Error> {
        let started = Instant::now();
        let method = req.method().clone();
        let uri = req.uri().clone();
        if let Some(res) = self.prepare_request(&mut req, started) {
            return Ok(res);
        }

        let (route, res) = self.dispatch_request(req).await;
        let status;
        let mut res = match res {
            Ok(res) => {
                status = res.status();
                info!("{method} {uri} {}", status.as_u16());
                res
            }
            Err(err) => {
                status = finalize_err_status(StatusCode::OK, &err);
                error!(
                    "{method} {uri} {} {}",
                    status.as_u16(),
                    crate::nats_access::sanitized_error_chain(&err)
                );
                dispatch_error_response(&err)
            }
        };
        *res.status_mut() = status;
        set_cors_header(&mut res);
        harnx_metrics::record_http_request(
            method.as_str(),
            route,
            res.status().as_u16(),
            started.elapsed(),
        );
        Ok(res)
    }

    /// Authorize before selecting a route, including direct attachment handlers.
    /// Return the metric label with the result so failures use the same response path.
    async fn dispatch_request(
        self: &Arc<Self>,
        mut req: hyper::Request<Incoming>,
    ) -> (&'static str, Result<AppResponse>) {
        if let Err(err) = self.guard_agent_request(&mut req).await {
            return ("/v1/agents/*", Err(guard_error_status(err)));
        }
        let method = req.method().clone();
        let uri = req.uri().clone();
        let path = uri.path();
        let route;
        let res = if path == "/v1/embeddings" {
            route = "/v1/embeddings";
            self.embeddings(req).await
        } else if path == "/v1/rerank" {
            route = "/v1/rerank";
            self.rerank(req).await
        } else if path == "/v1/models" {
            route = "/v1/models";
            self.list_models()
        } else if path == "/v1/agents" {
            route = "/v1/agents";
            self.list_agents(&req).await
        } else if is_session_attachment_blob_path(path) {
            route = "/v1/agents/*/sessions/*/attachments/*";
            self.get_session_attachment(req).await
        } else if is_session_attachments_path(path) {
            route = "/v1/agents/*/sessions/*/attachments";
            self.upload_session_attachments(req).await
        } else if let Some(resource_route) = cid::resource_route(path) {
            route = resource_route.metric_name();
            self.handle_resource_route(req, resource_route).await
        } else if path == "/v1/rags" {
            route = "/v1/rags";
            self.list_rags()
        } else if path == "/v1/rags/search" {
            route = "/v1/rags/search";
            self.search_rag(req).await
        } else if method == Method::GET || method == Method::HEAD {
            route = "other";
            self.serve_web_asset(&method, path, req.headers()).await
        } else {
            route = "other";
            Err(anyhow!("Not Found"))
        };
        (route, res)
    }

    fn list_models(&self) -> Result<AppResponse> {
        let data = json!({ "data": self.models });
        let res = Response::builder()
            .header("Content-Type", "application/json; charset=utf-8")
            .body(Full::new(Bytes::from(data.to_string())).boxed())?;
        Ok(res)
    }

    async fn filter_agents_by_role(&self, query: Option<&str>) -> Result<Vec<AgentConfig>> {
        // Filter by role FIRST, using canonical names, THEN normalize for display.
        // This keeps the role=assistant query working against list_assistant_agents()
        // which returns canonical `name@cluster` strings.
        let agents = if query_requests_assistant_role(query) {
            let assistant_names = harnx_runtime::config::agent::list_assistant_agents().await;
            self.agents
                .iter()
                .filter(|agent| assistant_names.iter().any(|name| name == agent.name()))
                .cloned()
                .collect()
        } else {
            self.agents.clone()
        };

        // Now apply display normalization
        Ok(self.normalize_agents_for_display(agents))
    }

    /// Normalize agent names for display in cluster-client mode.
    ///
    /// Agents on the default cluster are shown without the `@<cluster>` suffix.
    /// When a local agent and a default-cluster remote agent share the same bare name,
    /// the remote agent's metadata wins (bare names route to the default cluster).
    fn normalize_agents_for_display(&self, agents: Vec<AgentConfig>) -> Vec<AgentConfig> {
        let default_cluster = self.default_cluster_for_display();

        // Build map from display name -> agent config, preferring remote default-cluster
        // entries over local when they share the same bare name.
        let mut agent_map: std::collections::BTreeMap<String, (bool, AgentConfig)> =
            std::collections::BTreeMap::new();

        for agent in agents {
            let canonical_name = agent.name();
            let default_remote_bare = match (
                default_cluster,
                harnx_core::agent_ref::AgentRef::parse(canonical_name),
            ) {
                (Some(default), harnx_core::agent_ref::AgentRef::Remote { agent, cluster })
                    if cluster == default =>
                {
                    Some(agent.into_owned())
                }
                _ => None,
            };
            let is_default_remote = default_remote_bare.is_some();
            let display_name = default_remote_bare.unwrap_or_else(|| canonical_name.to_string());

            // A bare name routes to the default cluster in cluster-client mode,
            // so on a collision the default-cluster remote entry wins.
            use std::collections::btree_map::Entry;
            match agent_map.entry(display_name.clone()) {
                Entry::Vacant(e) => {
                    let mut config = agent.clone();
                    config.set_name(&display_name);
                    e.insert((is_default_remote, config));
                }
                Entry::Occupied(mut e) => {
                    if is_default_remote && !e.get().0 {
                        let mut config = agent.clone();
                        config.set_name(&display_name);
                        e.insert((is_default_remote, config));
                    }
                }
            }
        }

        agent_map.into_values().map(|(_, config)| config).collect()
    }

    /// Serve a static web-ui asset for a GET/HEAD request that did not match an
    /// API route. Maps the request path to a file under `self.web_assets`,
    /// guarding against path traversal. Returns 404 (`"Not Found"`) when the
    /// assets directory or the requested file is absent. For navigation
    /// requests (`Accept: text/html`) that do not name a concrete file, falls
    /// back to serving `index.html` to support single-page-app routing.
    async fn serve_web_asset(
        &self,
        method: &Method,
        path: &str,
        headers: &http::HeaderMap,
    ) -> Result<AppResponse> {
        // Canonicalize the root once; if it does not exist, nothing to serve.
        let root = match tokio::fs::canonicalize(&self.web_assets).await {
            Ok(root) => root,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => bail!("Not Found"),
            Err(err) => return Err(err.into()),
        };

        let request_path = path.trim_start_matches('/');
        let requested = match sanitize_asset_path(request_path) {
            Some(path) => path,
            None => bail!("Not Found"),
        };

        let candidate = if requested.as_os_str().is_empty() {
            root.join("index.html")
        } else {
            root.join(&requested)
        };

        if let Some(response) = self
            .serve_asset_candidate(method, &root, &candidate, requested.as_os_str().is_empty())
            .await?
        {
            return Ok(response);
        }

        // SPA fallback: navigation requests for a route (no file extension) get
        // index.html so client-side routing can take over.
        if wants_html(headers) && !has_file_extension(&requested) {
            let index = root.join("index.html");
            if let Some(response) = self
                .serve_asset_candidate(method, &root, &index, true)
                .await?
            {
                return Ok(response);
            }
        }

        bail!("Not Found")
    }

    /// Attempt to serve a single candidate file. Returns `Ok(None)` when the
    /// file does not exist or is not a regular file so the caller can try a
    /// fallback. Rejects any file whose canonical path escapes `root`.
    async fn serve_asset_candidate(
        &self,
        method: &Method,
        root: &Path,
        candidate: &Path,
        force_html: bool,
    ) -> Result<Option<AppResponse>> {
        let metadata = match tokio::fs::metadata(candidate).await {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };

        if !metadata.is_file() {
            return Ok(None);
        }

        // Defense in depth against symlinks/traversal: the resolved path must
        // stay within the canonical assets root.
        let canonical = match tokio::fs::canonicalize(candidate).await {
            Ok(path) => path,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        if !canonical.starts_with(root) {
            bail!("Not Found");
        }

        let content_type = if force_html {
            "text/html; charset=utf-8"
        } else {
            content_type_for_path(candidate)
        };

        let body = if *method == Method::HEAD {
            Bytes::new()
        } else {
            tokio::fs::read(&canonical).await?.into()
        };

        // Advertise the full file size even for HEAD so caches/proxies and
        // client probes see an accurate Content-Length.
        let response = Response::builder()
            .header("Content-Type", content_type)
            .header(http::header::CONTENT_LENGTH, metadata.len())
            .body(Full::new(body).boxed())?;
        Ok(Some(response))
    }

    async fn ag_ui_rpc_route(
        &self,
        req: hyper::Request<Incoming>,
        target: &ResolvedAgentTarget,
        session: &str,
    ) -> Result<AppResponse> {
        let context = RpcContext {
            config: &self.config,
            registry: &self.session_registry,
            persistence: PersistenceKind::Nats,
            web_base_url: web_url::inferred_base_url(req.headers(), req.uri()),
            user_id: RequestIdentity::user_id(&req),
            user_principals: RequestIdentity::of(&req).stampable_principals(),
            attachment_access: self.access_rules.as_deref().map(|rules| {
                crate::ag_ui_rpc::RequestAccess {
                    rules,
                    config: &self.config,
                    identity: RequestIdentity::of(&req),
                }
            }),
        };
        handle_ag_ui_rpc(req, target, session, context).await
    }

    /// The `/agents/<agent>/sessions` routes: the request's resolved identity
    /// is stamped read-only when a route creates a session.
    async fn handle_agents_sessions_route(
        &self,
        req: hyper::Request<Incoming>,
        target: &crate::session_actor::ResolvedAgentTarget,
        scoped: &harnx_runtime::config::GlobalConfig,
        access: Option<(&AccessRules, CallerView<'_>)>,
    ) -> Result<AppResponse> {
        // The query and identity are extracted as owned/locals so the request
        // itself can be handed to the collection route for body consumption.
        let query = req.uri().query().map(str::to_string);
        let identity = RequestIdentity::of(&req);
        let principals = identity.stampable_principals();
        let context = SessionsRouteContext::new(target, scoped, query.as_deref())
            .with_identity(identity.user_id.as_deref(), principals.as_ref())
            .with_access(access);
        self.handle_sessions_route(req, context).await
    }

    async fn handle_agent_tree(&self, req: hyper::Request<Incoming>) -> Result<AppResponse> {
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        let (rules, identities) = self.access(&req).unzip();
        let identities = identities.unwrap_or_default();
        let caller = identities.caller();
        let access = rules.map(|rules| (rules, caller.view()));
        if let Some((agent_ref, session, route)) =
            session_routes::parse_session_metadata_route(&path)
        {
            let (target, _) = self.resolve_request_agent(&req, &agent_ref).await?;
            let session_ref = AgentSessionRef::from_target(&target, &session);
            return self
                .handle_session_metadata_route(req, session_ref, route)
                .await;
        }
        let route = parse_agents_route(&path).ok_or_else(|| anyhow!("Not Found"))?;
        let (agent_ref, session_name, agent_route) = route;
        let (target, scoped) = self.resolve_request_agent(&req, &agent_ref).await?;
        let display_ref = self.display_ref(&target);

        match agent_route {
            AgentsRoute::Agent => {
                match negotiate_agents_route(&method, req.headers(), agent_route)? {
                    AgentsRepresentation::Html => self.agent_html_page(&display_ref),
                    AgentsRepresentation::Json => self.agent_json(&target, access).await,
                    AgentsRepresentation::AgUiSse | AgentsRepresentation::AgUiRpc => {
                        Err(anyhow!("Not Acceptable"))
                    }
                }
            }
            AgentsRoute::Sessions => {
                self.handle_agents_sessions_route(req, &target, &scoped, access)
                    .await
            }
            AgentsRoute::Session => {
                self.handle_agent_session(
                    req,
                    &target,
                    session_name.as_deref().expect("session route has a name"),
                    &scoped,
                )
                .await
            }
            AgentsRoute::SessionHistory => {
                let name = session_name.expect("history route has a session name");
                self.session_history_page(req, target, name).await
            }
            AgentsRoute::SessionEvents => {
                let session_name = session_name.expect("session events route has a session name");
                self.handle_session_events_route(
                    &method,
                    req.headers(),
                    AgentSessionRef::from_target(&target, &session_name),
                )
                .await
            }
        }
    }

    async fn session_history_page(
        &self,
        req: hyper::Request<Incoming>,
        target: ResolvedAgentTarget,
        session: String,
    ) -> Result<AppResponse> {
        negotiate_agents_route(req.method(), req.headers(), AgentsRoute::SessionHistory)?;
        let key = session_actor::SessionKey::new(target, session);
        session_history::http(
            req.uri().query(),
            (&self.config, &self.session_registry, key),
        )
        .await
    }

    async fn handle_agent_session(
        &self,
        req: hyper::Request<Incoming>,
        target: &ResolvedAgentTarget,
        session: &str,
        scoped: &harnx_runtime::config::GlobalConfig,
    ) -> Result<AppResponse> {
        // A deferred claim means the guard found no owner record and this
        // POST may create the session; classification, reservation and
        // authorization run before any dispatch (plan design steps 3-5).
        if let Some(claim) = req
            .extensions()
            .get::<crate::session_claim::DeferredSessionClaim>()
            .cloned()
        {
            return self.handle_deferred_claim(req, target, claim, scoped).await;
        }
        match negotiate_agents_route(req.method(), req.headers(), AgentsRoute::Session)? {
            AgentsRepresentation::Html => {
                self.session_html_page(&self.display_ref(target), session)
            }
            AgentsRepresentation::Json => self.session_history_json(target, session).await,
            AgentsRepresentation::AgUiSse => self.ag_ui_run_route(req, target, session).await,
            AgentsRepresentation::AgUiRpc => self.ag_ui_rpc_route(req, target, session).await,
        }
    }

    fn agent_html_page(&self, agent: &str) -> Result<AppResponse> {
        let html = format!("<h1>agent: {}</h1>", escape_html_text(agent));
        let res = Response::builder()
            .header("Content-Type", "text/html; charset=utf-8")
            .body(Full::new(Bytes::from(html)).boxed())?;
        Ok(res)
    }

    fn session_html_page(&self, _agent: &str, session: &str) -> Result<AppResponse> {
        let html = format!("<h1>session: {}</h1>", escape_html_text(session));
        let res = Response::builder()
            .header("Content-Type", "text/html; charset=utf-8")
            .body(Full::new(Bytes::from(html)).boxed())?;
        Ok(res)
    }

    async fn agent_json(
        &self,
        target: &ResolvedAgentTarget,
        access: Option<(&AccessRules, CallerView<'_>)>,
    ) -> Result<AppResponse> {
        let sessions = agent_sessions_json(&self.config, target, access).await?;
        let display_ref = self.display_ref(target);
        // Look up description using both display name and full @cluster name
        let description = self
            .agents
            .iter()
            .find(|candidate| {
                candidate.name() == display_ref || candidate.name() == target.display_ref()
            })
            .map(AgentConfig::description)
            .filter(|description| !description.is_empty());
        let data = json!({
            "name": display_ref,
            "description": description,
            "sessions": sessions,
        });
        json_response(data)
    }

    async fn sessions_json(
        &self,
        target: &ResolvedAgentTarget,
        params: Option<session_pagination::SessionPaginationQuery>,
        access: Option<(&AccessRules, CallerView<'_>)>,
    ) -> Result<AppResponse> {
        let params = match params {
            Some(params) => params,
            None => {
                return json_response(Value::Array(
                    agent_sessions_json(&self.config, target, access).await?,
                ));
            }
        };
        let sessions = list_target_sessions(&self.config, target, access).await?;
        let data = session_pagination::paginate_sessions(&sessions, &params)?;
        json_response(data)
    }

    async fn get_session_attachment<B>(&self, req: hyper::Request<B>) -> Result<AppResponse> {
        if req.method() != Method::GET {
            bail!("Method Not Allowed");
        }
        let (agent_ref, session, cid) = parse_session_attachment_blob_path(req.uri().path())
            .ok_or_else(|| anyhow!("Not Found"))?;
        if !is_canonical_attachment_cid(cid.as_bytes()) {
            return attachment_error_response(StatusCode::BAD_REQUEST, "malformed attachment cid");
        }

        let (target, _) = self.resolve_request_agent(&req, &agent_ref).await?;
        let (loaded_session, _entries) = load_nats_session(&self.config, &target, &session).await?;
        if loaded_session.agent_name.as_deref() != Some(target.agent())
            || !collect_cid_refs(&loaded_session.messages)
                .iter()
                .any(|member| member == &cid)
        {
            return attachment_error_response(StatusCode::NOT_FOUND, "attachment not found");
        }

        let attachments_dir = Config::agent_data_dir(target.agent())
            .join("attachments")
            .join(&session);
        let local_cid = match CidUrl::parse(&cid) {
            Ok(CidUrl::Media { hash, .. }) => format!("{CID_PREFIX}{hash}"),
            _ => unreachable!("canonical attachment cid was validated above"),
        };
        let jetstream = serve_nats_jetstream(&self.config, target.cluster()).await?;
        let attachment =
            read_cached_or_nats_attachment(&attachments_dir, &local_cid, &jetstream, &cid).await?;
        let Some((bytes, mime_type)) = attachment else {
            return attachment_error_response(StatusCode::NOT_FOUND, "attachment not found");
        };
        if !is_inline_image_mime(mime_type.as_bytes()) {
            return attachment_error_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "attachment content type cannot be displayed inline",
            );
        }

        let response = Response::builder()
            .status(StatusCode::OK)
            .header(http::header::CONTENT_TYPE, mime_type)
            .header("X-Content-Type-Options", "nosniff")
            .header(http::header::CACHE_CONTROL, "private, max-age=86400")
            .body(Full::new(Bytes::from(bytes)).boxed())?;
        Ok(response)
    }

    /// Park the current upload part between its acknowledged put and its
    /// session-incarnation recheck when the F6 test gate is armed; a no-op
    /// otherwise.
    async fn wait_at_upload_recheck_gate(&self) {
        self.upload_recheck_gate.wait().await;
    }

    /// The broker connection and the `harnx_attachments` bucket for uploads
    /// to one resolved cluster, from the Server-owned cache: the first upload
    /// to a cluster connects and ensures single-flight, every later upload
    /// clones the cached handles. For the local cluster the shared frontend
    /// broker must be running before any JetStream I/O; failing to bring it
    /// up is an availability failure, not a server fault. A successful
    /// connect is cached even when the bucket ensure fails — the working
    /// connection must not be discarded — but the bucket memo is cached only
    /// with a successful ensure. No lock is held across the I/O other than
    /// this cluster's single-flight init, so puts never serialize.
    // qual:allow(iosp) reason: "the single-flight init reads its own state and must classify its own ensure failures in place; the Err arm has to move the storage in before handing the client to the classifier, and splitting it would scatter which state a failing request leaves behind."
    async fn attachments_bucket(
        &self,
        target: &ResolvedAgentTarget,
    ) -> Result<(async_nats::Client, AttachmentsBucket)> {
        let cluster = target.cluster();
        if let Err(error) = ensure_frontend_nats_owner(cluster).await {
            return Err(nats_cluster_unavailable(cluster, error));
        }
        let entry = self.upload_storage.entry(cluster).await;
        let mut state = entry.storage.lock().await;

        if let Some(storage) = state.as_mut() {
            self.cached_upload_bucket(&entry, storage, cluster).await
        } else {
            let jetstream = serve_nats_jetstream(&self.config, cluster).await?;
            upload_storage::count_connect(&entry);
            match self.ensure_upload_bucket(&entry, &jetstream, cluster).await {
                Ok(bucket) => {
                    let storage =
                        upload_storage::ClusterStorage::new(jetstream, Some(bucket.clone()));
                    let client = storage.client.clone();
                    *state = Some(storage);
                    Ok((client, bucket))
                }
                Err(error) => {
                    // Keep the working connection, leave the memo unset so the
                    // next upload retries the ensure, and fail this request.
                    let storage = upload_storage::ClusterStorage::new(jetstream, None);
                    let client = storage.client.clone();
                    *state = Some(storage);
                    Err(Self::classify_upload_ensure_failure(
                        client.connection_state(),
                        cluster,
                        error,
                    ))
                }
            }
        }
    }

    /// The bucket for a cluster with a cached connection: straight from the
    /// memo while it is fresh enough, otherwise re-ensured (the bounded-rate
    /// retry of a failed best-effort replica raise), or re-ensured on the
    /// kept connection when the memo was reset because the stream was gone.
    // qual:allow(iosp) reason: "one ordered memo decision: serve while fresh, re-raise past the interval, otherwise fall to the unset-memo retry below; each branch names a different memo state and splitting them hides which state serves which failure."
    async fn cached_upload_bucket(
        &self,
        entry: &upload_storage::ClusterEntry,
        storage: &mut upload_storage::ClusterStorage,
        cluster: &str,
    ) -> Result<(async_nats::Client, AttachmentsBucket)> {
        let client = storage.client.clone();
        if let Some(bucket) = storage.bucket.clone() {
            if storage.ensured_at.elapsed() < self.upload_bucket_recheck {
                return Ok((client, bucket));
            }
            // The memo is old enough that the best-effort replica raise
            // should be retried. A failed re-ensure keeps the old memo
            // serving — a raise that cannot succeed now must not break
            // uploads — and pushes its next retry past the recheck
            // interval instead of once per upload.
            match self
                .ensure_upload_bucket(entry, &storage.jetstream, cluster)
                .await
            {
                Ok(fresh) => {
                    storage.bucket = Some(fresh.clone());
                    storage.ensured_at = std::time::Instant::now();
                    Ok((client, fresh))
                }
                Err(error) => {
                    storage.ensured_at = std::time::Instant::now();
                    log::warn!(
                        "re-ensuring the upload bucket for cluster '{cluster}' failed; \
                         keeping the cached bucket: {}",
                        crate::nats_access::sanitized_error_chain(&error)
                    );
                    Ok((client, bucket))
                }
            }
        } else {
            self.ensure_unmemoed_upload_bucket(entry, storage, cluster)
                .await
        }
    }

    /// The memo-unset path: the last ensure failed (or the stream was
    /// reset), so there is no bucket to serve. A connected broker retries
    /// per upload — store-level failures fail fast (D2). A down
    /// connection defers unconditionally instead: an ensure attempt could
    /// only queue behind the single-flight lock until the client's request
    /// timeout, so every upload during an outage gets an instant 503
    /// rather than a slow one, and nothing retries until the connection is
    /// back (C7) — at which point recovery is immediate.
    // qual:allow(iosp) reason: "the deferral check, the ensure attempt and its memo bookkeeping must read as one retry step; extracting the bookkeeping would let a success path forget to record the fresh ensure."
    async fn ensure_unmemoed_upload_bucket(
        &self,
        entry: &upload_storage::ClusterEntry,
        storage: &mut upload_storage::ClusterStorage,
        cluster: &str,
    ) -> Result<(async_nats::Client, AttachmentsBucket)> {
        let connected =
            storage.client.connection_state() == async_nats::connection::State::Connected;
        if !connected {
            return Err(nats_cluster_unavailable(
                cluster,
                anyhow::anyhow!("broker connection is down; upload bucket ensure deferred"),
            ));
        }
        let client = storage.client.clone();
        match self
            .ensure_upload_bucket(entry, &storage.jetstream, cluster)
            .await
        {
            Ok(bucket) => {
                storage.bucket = Some(bucket.clone());
                storage.ensured_at = std::time::Instant::now();
                Ok((client, bucket))
            }
            Err(error) => Err(Self::classify_upload_ensure_failure(
                storage.client.connection_state(),
                cluster,
                error,
            )),
        }
    }

    /// Resolve the configured replica count and ensure the bucket on one
    /// cluster's connection, counting each attempt for the reuse and
    /// retry-rate test seams (an attempt that fails still counts: the
    /// rate-limit test must observe attempts, not successes).
    async fn ensure_upload_bucket(
        &self,
        entry: &upload_storage::ClusterEntry,
        jetstream: &async_nats::jetstream::Context,
        cluster: &str,
    ) -> Result<AttachmentsBucket> {
        let replicas = self.config.resolve_nats_replicas(cluster).await?;
        upload_storage::count_ensure(entry);
        let bucket =
            harnx_blob_store::media::ensure_attachments_bucket(jetstream, replicas).await?;
        Ok(bucket)
    }

    /// A failed bucket ensure has no typed classification of its own, but a
    /// failure on a lost or reconnecting connection is unavailability, same
    /// as a failed put (F3); on a live connection the wrapper kinds in
    /// `is_nats_unavailable` classify the rest. The original chain is
    /// logged by `nats_cluster_unavailable`, not returned to the client.
    /// Takes the connection state, not the client, so tests cover the
    /// branch without a broker.
    fn classify_upload_ensure_failure(
        connection: async_nats::connection::State,
        cluster: &str,
        error: anyhow::Error,
    ) -> anyhow::Error {
        if connection != async_nats::connection::State::Connected {
            return nats_cluster_unavailable(cluster, error);
        }
        error
    }

    /// After a store-class put failure, drop the cluster's bucket memo when
    /// the object-store stream is definitively gone, so the next upload
    /// re-ensures it on the kept connection. Transient probe failures keep
    /// the memo: only the typed stream-not-found answer resets it.
    async fn refresh_upload_bucket_on_missing_stream(&self, cluster: &str) {
        let entry = self.upload_storage.entry(cluster).await;
        let jetstream = {
            let state = entry.storage.lock().await;
            match state.as_ref() {
                Some(storage) => storage.jetstream.clone(),
                None => return,
            }
        };
        let stream_name = harnx_blob_store::media::attachments_stream_name();
        match jetstream.get_stream(&stream_name).await {
            Err(error) if harnx_blob_store::media::attachments_stream_missing(&error.kind()) => {
                let mut state = entry.storage.lock().await;
                if let Some(storage) = state.as_mut() {
                    if storage.bucket.take().is_some() {
                        log::warn!(
                            "attachment object store '{stream_name}' is gone; \
                             the next upload re-ensures it"
                        );
                    }
                }
            }
            // A live stream (or a probe that could not answer) keeps the memo.
            _ => {}
        }
    }

    /// Test seam: shrink the bucket recheck interval to observe re-ensures.
    #[cfg(test)]
    pub(crate) fn set_upload_bucket_recheck_interval(&mut self, interval: Duration) {
        self.upload_bucket_recheck = interval;
    }

    /// Test seam: this cluster's successful-connect and bucket-ensure counts.
    #[cfg(test)]
    pub(crate) async fn upload_storage_counts(&self, cluster: &str) -> (usize, usize) {
        let entry = self.upload_storage.entry(cluster).await;
        (
            entry.connects.load(std::sync::atomic::Ordering::Relaxed),
            entry.ensures.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Test seam: the cached connection's state for one cluster, `None`
    /// until the first upload initializes its storage.
    #[cfg(test)]
    pub(crate) async fn upload_connection_state(
        &self,
        cluster: &str,
    ) -> Option<async_nats::connection::State> {
        let entry = self.upload_storage.entry(cluster).await;
        let storage = entry.storage.lock().await;
        storage
            .as_ref()
            .map(|storage| storage.client.connection_state())
    }

    async fn upload_session_attachments<B>(&self, req: hyper::Request<B>) -> Result<AppResponse>
    where
        B: Body<Data = Bytes> + Send + Unpin,
        <B as Body>::Error: std::fmt::Display,
    {
        let (agent_ref, session, boundary) = upload_route(&req)?;
        let (target, _) = self.resolve_request_agent(&req, &agent_ref).await?;
        let attachments_dir = Config::agent_data_dir(target.agent())
            .join("attachments")
            .join(&session);
        let attachment_session = SessionRef::new(Some(target.agent().to_string()), session.clone())
            .context("build attachment session identity")?;
        // With access rules on, the guard already proved this session exists;
        // pin that proof before the body buffers so a session deleted during
        // the upload is a 404 instead of a never-created (D7) upload that no
        // recheck guards (M2). Rules off keeps D7: no proof, no extra read.
        let proven_incarnation = match self
            .upload_session_incarnation_proof(&req, &target, &session)
            .await
        {
            Ok(proof) => proof,
            Err(error) => {
                let (status, body) = attachment_store_error(error);
                return json_response_with_status(status, body);
            }
        };
        let proven_incarnation = match upload_proven_incarnation(proven_incarnation) {
            Ok(proven) => proven,
            Err((status, body)) => return json_response_with_status(status, body),
        };
        // JetStream is taken lazily on the first accepted part, so validation
        // failures below never need a broker and keep their 4xx — until a
        // part is accepted, the broker stays untouched, but a stored part has
        // used it and later parts' validation errors may follow a store
        // failure.
        let mut object_store =
            AttachmentObjectStore::new(self, &target, &attachment_session, proven_incarnation);

        // The headers must outlive the body, which consumes the request.
        let headers = http::HeaderMap::clone(req.headers());
        let body_bytes = match read_upload_body(&headers, req.into_body()).await {
            Ok(bytes) => bytes,
            Err((status, body)) => return json_response_with_status(status, body),
        };

        let refs = match process_multipart_upload(
            body_bytes,
            boundary,
            &attachments_dir,
            &attachment_session,
            &mut object_store,
        )
        .await?
        {
            Ok(refs) => refs,
            Err((status, body)) => return json_response_with_status(status, body),
        };
        json_response(
            json!({"attachment_refs": refs, "attachments": refs.iter().map(|cid| json!({"cid": cid})).collect::<Vec<_>>() }),
        )
    }

    /// The session-incarnation proof an upload starts from. Access rules off
    /// proves nothing (`None`): the upload captures with D7 semantics. Rules
    /// on re-reads the session's `created_at` before the request's body
    /// buffers, so the upload can tell "deleted mid-upload" from "never
    /// created". `Some(None)` means the rules-on path found nothing this
    /// caller may upload to: the record is already absent, or the record
    /// now under this session id has an owner the caller cannot access —
    /// the session was replaced between the guard's authorization and this
    /// read, and pinning that incarnation would let the upload store for a
    /// session the guard never approved. Either way the upload answers 404
    /// before storing anything. Read failures propagate to the caller and
    /// classify like any other store failure (503/500, no refs).
    ///
    /// The rules-on proof opens a second, short-lived connection (D9): the
    /// upload store's per-cluster cached client is private to the storage
    /// lock and only holds the attachments bucket, not the session-metadata
    /// store this proof needs. Reusing it would widen that lock's reach
    /// across two unrelated stores; one extra connection per rules-on
    /// upload is the cheaper trade.
    async fn upload_session_incarnation_proof<B>(
        &self,
        req: &hyper::Request<B>,
        target: &ResolvedAgentTarget,
        session: &str,
    ) -> Result<Option<Option<DateTime<Utc>>>> {
        let Some((rules, identities)) = self.access(req) else {
            return Ok(None);
        };
        let Some(store) =
            crate::session_access::open_session_metadata_readonly(&self.config, target.cluster())
                .await?
        else {
            // No metadata bucket: with rules on no session can exist, so the
            // session this upload addresses is gone.
            return Ok(Some(None));
        };
        let caller = identities.caller();
        let display_ref = self.display_ref(target);
        let pinned = match store.get_for_agent(session, target.agent()).await {
            Ok(Some(record)) => {
                Self::proof_from_record(rules, &display_ref, caller.view(), &record)
            }
            Ok(None) => None,
            Err(error) => {
                return Err(error.context("read the session's metadata for the upload proof"))
            }
        };
        Ok(Some(pinned))
    }

    /// The proof's guard-equivalent authorization: the caller must still be
    /// able to access the record about to be pinned, judged by that record's
    /// own owner — the same check the guard ran on its own read, with the
    /// rules and caller snapshot resolved from this request. `None` means
    /// the caller cannot access this record, so nothing is pinned. An
    /// undecodable record cannot prove anyone's authorization and denies
    /// the same way (fail closed, 404) instead of surfacing a 500.
    // qual:allow(iosp) reason: "The proof must decode the record, say why an undecodable one is denied, and apply the caller's authorization against the same record's owner in one pass; splitting the deny from the authorize scatters which read decided the 404."
    fn proof_from_record(
        rules: &harnx_core::access_rules::AccessRules,
        display_ref: &str,
        caller: harnx_core::access_rules::CallerView<'_>,
        record: &harnx_runtime::nats_session_metadata::MetadataRecord,
    ) -> Option<DateTime<Utc>> {
        let properties =
            match harnx_runtime::nats_session_metadata::session_properties(&record.metadata) {
                Ok(properties) => properties,
                Err(error) => {
                    // Fail closed, but say why in the log: a corrupted
                    // record should be visible, not indistinguishable from
                    // a normal 404. Sanitized — the record is untrusted.
                    warn!(
                        "undecodable session record in the upload proof; failing closed: {}",
                        crate::nats_access::sanitized_error_chain(&error)
                    );
                    return None;
                }
            };
        let owner = properties.text("user_id");
        rules
            .can_access_session(display_ref, caller, owner)
            .then_some(record.metadata.created_at)
    }

    async fn session_history_json(
        &self,
        target: &ResolvedAgentTarget,
        session: &str,
    ) -> Result<AppResponse> {
        let (loaded_session, _entries) = load_nats_session(&self.config, target, session).await?;
        if loaded_session.agent_name.as_deref() != Some(target.agent()) {
            bail!("Not Found");
        }

        let mut seq_counts = BTreeMap::new();
        let messages = loaded_session
            .messages
            .iter()
            .map(|message| {
                let id = history_message_id(message, &mut seq_counts);
                json!({
                    "id": id,
                    "role": history_role_name(message.role),
                    "content": history_message_content(message),
                })
            })
            .collect();
        json_response(Value::Array(messages))
    }

    async fn ag_ui_run_route(
        &self,
        req: hyper::Request<Incoming>,
        target: &ResolvedAgentTarget,
        session: &str,
    ) -> Result<AppResponse> {
        let user_id = RequestIdentity::user_id(&req);
        let user_principals = RequestIdentity::of(&req).stampable_principals();
        let web_base_url = web_url::inferred_base_url(req.headers(), req.uri());
        let snapshot_features = transcript_snapshot::SnapshotFeatures::from_headers(req.headers());
        let body = req.collect().await?.to_bytes();
        let request = ag_ui::AgUiRunRequest {
            target,
            session,
            body: &body,
            snapshot_features,
            web_base_url,
            user_id,
            user_principals,
        };
        ag_ui::ag_ui_run_for_target(
            &self.config,
            &self.session_registry,
            request,
            self.shutdown.clone(),
        )
        .await
        .map_err(ag_ui_error_to_anyhow)
    }

    fn list_rags(&self) -> Result<AppResponse> {
        let data = json!({ "data": self.rags });
        let res = Response::builder()
            .header("Content-Type", "application/json; charset=utf-8")
            .body(Full::new(Bytes::from(data.to_string())).boxed())?;
        Ok(res)
    }

    async fn search_rag(&self, req: hyper::Request<Incoming>) -> Result<AppResponse> {
        let req_body = req.collect().await?.to_bytes();
        let req_body: Value = serde_json::from_slice(&req_body)
            .map_err(|err| anyhow!("Invalid request json, {err}"))?;

        debug!("search rag request: {req_body}");
        let SearchRagReqBody { name, input } = serde_json::from_value(req_body)
            .map_err(|err| anyhow!("Invalid request body, {err}"))?;

        let config = Arc::new(ConfigLock::new(self.config.clone()));

        let abort_signal = create_abort_signal();

        let rag_path = config.read().rag_file(&name);
        let rag = Rag::load(&config.read().clients, &name, &rag_path)?;

        let rag_result = Config::search_rag(&config, &rag, &input, abort_signal).await?;

        let data = json!({ "data": rag_result });
        let res = Response::builder()
            .header("Content-Type", "application/json; charset=utf-8")
            .body(Full::new(Bytes::from(data.to_string())).boxed())?;
        Ok(res)
    }

    async fn embeddings(&self, req: hyper::Request<Incoming>) -> Result<AppResponse> {
        let req_body = req.collect().await?.to_bytes();
        let req_body: Value = serde_json::from_slice(&req_body)
            .map_err(|err| anyhow!("Invalid request json, {err}"))?;

        debug!("embeddings request: {req_body}");
        let req_body = serde_json::from_value(req_body)
            .map_err(|err| anyhow!("Invalid request body, {err}"))?;

        let EmbeddingsReqBody {
            input,
            model: embedding_model_id,
        } = req_body;

        let config = Arc::new(ConfigLock::new(self.config.clone()));

        let embedding_model = harnx_runtime::client::retrieve_model(
            &config.read().clients,
            &embedding_model_id,
            ModelType::Embedding,
        )?;

        let texts = match input {
            EmbeddingsReqBodyInput::Single(v) => vec![v],
            EmbeddingsReqBodyInput::Multiple(v) => v,
        };
        let client = init_client(&config.read().clients, &embedding_model)?;
        let (emb_dry_run, emb_ua) = {
            let cfg = config.read();
            (cfg.dry_run, cfg.user_agent.clone())
        };
        let emb_ctx = harnx_runtime::client::ClientCallContext {
            user_agent: emb_ua.as_deref(),
            dry_run: emb_dry_run,
        };
        let data = client
            .embeddings(
                &EmbeddingsData {
                    query: false,
                    texts,
                },
                &emb_ctx,
            )
            .await?;
        let data: Vec<_> = data
            .into_iter()
            .enumerate()
            .map(|(i, v)| {
                json!({
                        "object": "embedding",
                        "embedding": v,
                        "index": i,
                })
            })
            .collect();
        let output = json!({
            "object": "list",
            "data": data,
            "model": embedding_model_id,
            "usage": {
                "prompt_tokens": 0,
                "total_tokens": 0,
            }
        });
        let res = Response::builder()
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(output.to_string())).boxed())?;
        Ok(res)
    }

    async fn rerank(&self, req: hyper::Request<Incoming>) -> Result<AppResponse> {
        let req_body = req.collect().await?.to_bytes();
        let req_body: Value = serde_json::from_slice(&req_body)
            .map_err(|err| anyhow!("Invalid request json, {err}"))?;

        debug!("rerank request: {req_body}");
        let req_body = serde_json::from_value(req_body)
            .map_err(|err| anyhow!("Invalid request body, {err}"))?;

        let RerankReqBody {
            model: reranker_model_id,
            documents,
            query,
            top_n,
        } = req_body;

        let top_n = top_n.unwrap_or(documents.len());

        let config = Arc::new(ConfigLock::new(self.config.clone()));

        let reranker_model = harnx_runtime::client::retrieve_model(
            &config.read().clients,
            &reranker_model_id,
            ModelType::Reranker,
        )?;

        let client = init_client(&config.read().clients, &reranker_model)?;
        let (rr_dry_run, rr_ua) = {
            let cfg = config.read();
            (cfg.dry_run, cfg.user_agent.clone())
        };
        let rr_ctx = harnx_runtime::client::ClientCallContext {
            user_agent: rr_ua.as_deref(),
            dry_run: rr_dry_run,
        };
        let data = client
            .rerank(
                &RerankData {
                    query,
                    documents: documents.clone(),
                    top_n,
                },
                &rr_ctx,
            )
            .await?;

        let results: Vec<_> = data
            .into_iter()
            .map(|v| {
                json!({
                    "index": v.index,
                    "relevance_score": v.relevance_score,
                    "document": documents.get(v.index).map(|v| json!(v)).unwrap_or_default(),
                })
            })
            .collect();
        let output = json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "results": results,
        });
        let res = Response::builder()
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(output.to_string())).boxed())?;
        Ok(res)
    }
}

#[derive(Debug, Deserialize)]
struct SearchRagReqBody {
    name: String,
    input: String,
}

#[derive(Debug, Deserialize)]
struct EmbeddingsReqBody {
    input: EmbeddingsReqBodyInput,
    model: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum EmbeddingsReqBodyInput {
    Single(String),
    Multiple(Vec<String>),
}

#[derive(Debug, Deserialize)]
struct RerankReqBody {
    documents: Vec<String>,
    query: String,
    model: String,
    top_n: Option<usize>,
}

/// Reduce the upload's session-incarnation proof to the incarnation the
/// store should pin, or the 404 a rules-on upload owes a session whose
/// record is already absent at request start: the guard path would have
/// answered 404, so the upload does too, before reading or storing anything.
fn upload_proven_incarnation(
    proof: Option<Option<DateTime<Utc>>>,
) -> std::result::Result<Option<DateTime<Utc>>, AttachmentUploadError> {
    match proof {
        // No proof (rules off): the upload captures with D7 semantics.
        None => Ok(None),
        // The session vanished between the guard and the route's read.
        Some(None) => Err((
            StatusCode::NOT_FOUND,
            json!({"error": "session no longer exists"}),
        )),
        Some(proven) => Ok(proven),
    }
}

/// Method, path and multipart-boundary checks for the upload route. Body-size
/// enforcement happens in `read_upload_body`, before the body is read.
fn upload_route<B>(req: &hyper::Request<B>) -> Result<(String, String, String)> {
    if req.method() != Method::POST {
        bail!("Method Not Allowed");
    }
    let path = req.uri().path().to_string();
    let (agent_ref, session) =
        parse_session_attachments_path(&path).ok_or_else(|| anyhow!("Not Found"))?;
    let boundary = multipart_boundary(req.headers())?;
    Ok((agent_ref, session, boundary))
}

/// The multipart boundary of the request's Content-Type header.
fn multipart_boundary(headers: &http::HeaderMap) -> Result<String> {
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow!("Bad Request"))?;
    multer::parse_boundary(content_type).map_err(|_| anyhow!("Bad Request"))
}

/// Read the whole upload body, refusing anything over the size cap with the
/// route's 413 response before any multipart parsing.
async fn read_upload_body<B>(
    headers: &http::HeaderMap,
    body: B,
) -> std::result::Result<Vec<u8>, AttachmentUploadError>
where
    B: Body<Data = Bytes> + Send + Unpin,
    <B as Body>::Error: std::fmt::Display,
{
    if request_is_oversized(headers) {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"error":"payload too large","max_bytes":MAX_UPLOAD_BYTES}),
        ));
    }
    // Stream chunks with a running size limit to prevent OOM.
    let mut cumulative: usize = 0;
    let mut chunks: Vec<Bytes> = Vec::new();
    let mut body_stream = body.into_data_stream();

    while let Some(chunk_result) = body_stream.next().await {
        let chunk = chunk_result.map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                json!({"error": format!("Failed to read body: {e}")}),
            )
        })?;
        cumulative += chunk.len();
        if cumulative > MAX_UPLOAD_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                json!({"error":"payload too large","max_bytes":MAX_UPLOAD_BYTES}),
            ));
        }
        chunks.push(chunk);
    }
    Ok(chunks.into_iter().flatten().collect())
}

fn set_cors_header(res: &mut AppResponse) {
    res.headers_mut().insert(
        hyper::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        hyper::header::HeaderValue::from_static("*"),
    );
    res.headers_mut().insert(
        hyper::header::ACCESS_CONTROL_ALLOW_METHODS,
        hyper::header::HeaderValue::from_static("GET,POST,PUT,PATCH,DELETE"),
    );
    res.headers_mut().insert(
        hyper::header::ACCESS_CONTROL_ALLOW_HEADERS,
        hyper::header::HeaderValue::from_static(
            "Content-Type,Authorization,x-harnx-snapshot-features",
        ),
    );
}

/// A server-authored HTTP error annotation: the public message, the final
/// HTTP status, and an optional machine-readable `error.code`. It is
/// constructed only by the `annotate_error_message`/`error_with_status`
/// helpers (plus `guard_error_status`, which wraps broker-unavailability
/// failures in a 503 frame), and `status_from_error`/
/// `dispatch_error_response` read it through `anyhow::Error::downcast_ref`,
/// so raw error text — however it is shaped, including caller-controlled
/// agent refs, query keys and model IDs — can never supply or spoof a
/// status or code (#2955). The message is stored and displayed verbatim,
/// so URL-credential log redaction always sees the original text.
#[derive(Debug)]
pub(crate) struct AnnotatedError {
    pub(crate) message: String,
    pub(crate) status: StatusCode,
    pub(crate) code: Option<&'static str>,
}

// qual:allow(dry, boilerplate) reason: "anyhow carries the frame through Display, which must render the server-authored message verbatim; a derive dependency or single-use macro for one impl costs more than the impl itself (#2955)."
impl std::fmt::Display for AnnotatedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AnnotatedError {}

fn annotated<T: std::fmt::Display>(
    message: T,
    status: StatusCode,
    code: Option<&'static str>,
) -> AnnotatedError {
    AnnotatedError {
        message: message.to_string(),
        status,
        code,
    }
}

/// Annotates a message with an HTTP status, plus `error.code` when the error
/// carries one. Every `AnnotatedError` comes through here, except the guard's
/// 503 context wrapper (`guard_error_status`).
fn annotate_error_message<T: std::fmt::Display>(
    message: T,
    status: StatusCode,
    code: Option<&'static str>,
) -> anyhow::Error {
    anyhow::Error::new(annotated(message, status, code))
}

/// Annotates a message with only the HTTP status.
pub(crate) fn error_with_status<T: std::fmt::Display>(
    message: T,
    status: StatusCode,
) -> anyhow::Error {
    annotate_error_message(message, status, None)
}

/// Builds the JSON HTTP error body. `code` appears only when a
/// server-authored `AnnotatedError` supplied it.
// qual:allow(complexity, error_handling) reason: "the builder inputs are a static content-type and an in-memory body, so the build cannot fail."
fn error_body(message: &str, code: Option<&str>) -> AppResponse {
    let mut data = json!({
        "error": {
            "message": message,
            "type": "invalid_request_error",
        },
    });
    if let Some(code) = code {
        data["error"]["code"] = json!(code);
    }
    Response::builder()
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(data.to_string())).boxed())
        .unwrap()
}

/// Renders a raw message as the HTTP error body. Raw text is never parsed
/// for status or code annotations — those live in `AnnotatedError`, not in
/// message text — so caller-controlled strings cannot inject a code (#2955).
fn ret_err<T: std::fmt::Display>(err: T) -> AppResponse {
    error_body(&err.to_string(), None)
}

/// Renders the HTTP error body for a failed dispatch: a server-authored
/// annotation supplies the message and optional `error.code`; a raw error
/// renders its display text with no code. Raw text never reaches the
/// annotation fields.
fn dispatch_error_response(err: &anyhow::Error) -> AppResponse {
    // qual:allow(coupling, deh) reason: "the downcast is the trusted provenance boundary: only a typed AnnotatedError frame supplies status/code, so raw text can never be parsed as an annotation (#2955)."
    match err.downcast_ref::<AnnotatedError>() {
        Some(annotated) => error_body(&annotated.message, annotated.code),
        None => error_body(&err.to_string(), None),
    }
}
pub(crate) fn percent_decode(input: &str) -> String {
    let mut bytes = Vec::with_capacity(input.len());
    let mut iter = input.bytes();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let hi = iter.next().and_then(hex_val);
            let lo = iter.next().and_then(hex_val);
            if let (Some(h), Some(l)) = (hi, lo) {
                bytes.push(h << 4 | l);
            } else {
                bytes.push(b'%');
            }
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_agents_route(path: &str) -> Option<RouteMatch> {
    let suffix = path.strip_prefix("/v1/agents/")?;
    let segments: Vec<_> = suffix
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    match segments.as_slice() {
        [agent] => Some((percent_decode(agent), None, AgentsRoute::Agent)),
        [agent, "sessions"] => Some((percent_decode(agent), None, AgentsRoute::Sessions)),
        [agent, "sessions", session] => Some((
            percent_decode(agent),
            Some(percent_decode(session)),
            AgentsRoute::Session,
        )),
        [agent, "sessions", session, "history"] => Some((
            percent_decode(agent),
            Some(percent_decode(session)),
            AgentsRoute::SessionHistory,
        )),
        [agent, "sessions", session, "events"] => Some((
            percent_decode(agent),
            Some(percent_decode(session)),
            AgentsRoute::SessionEvents,
        )),
        _ => None,
    }
}

fn query_requests_assistant_role(query: Option<&str>) -> bool {
    let Some(query) = query else {
        return false;
    };

    query.split('&').any(|pair| {
        let mut parts = pair.splitn(2, '=');
        matches!(
            (parts.next(), parts.next()),
            (Some("role"), Some("assistant"))
        )
    })
}

fn is_session_attachments_path(path: &str) -> bool {
    let Some(suffix) = path.strip_prefix("/v1/agents/") else {
        return false;
    };
    let segments: Vec<_> = suffix
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    matches!(
        segments.as_slice(),
        [_agent, "sessions", _session, "attachments"]
    )
}

fn is_session_attachment_blob_path(path: &str) -> bool {
    let Some(suffix) = path.strip_prefix("/v1/agents/") else {
        return false;
    };
    let segments: Vec<_> = suffix
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    matches!(
        segments.as_slice(),
        [_agent, "sessions", _session, "attachments", _cid]
    )
}

fn negotiate_agents_route(
    method: &Method,
    headers: &http::HeaderMap,
    route: AgentsRoute,
) -> Result<AgentsRepresentation> {
    match (method, route) {
        (&Method::GET, AgentsRoute::Agent | AgentsRoute::Session) => {
            if accepts_html(headers) {
                Ok(AgentsRepresentation::Html)
            } else {
                Ok(AgentsRepresentation::Json)
            }
        }
        (&Method::GET, AgentsRoute::SessionHistory) => Ok(AgentsRepresentation::Json),
        (&Method::GET | &Method::POST, AgentsRoute::Sessions) => Ok(AgentsRepresentation::Json),
        (&Method::GET, AgentsRoute::SessionEvents) if accepts_event_stream(headers) => {
            Ok(AgentsRepresentation::AgUiSse)
        }
        (&Method::POST, AgentsRoute::Session) => {
            if accepts_event_stream(headers) {
                Ok(AgentsRepresentation::AgUiSse)
            } else if content_type_is_json(headers) {
                Ok(AgentsRepresentation::AgUiRpc)
            } else {
                Err(anyhow!("Not Acceptable"))
            }
        }
        _ => Err(anyhow!("Method Not Allowed")),
    }
}

fn accepts_html(headers: &http::HeaderMap) -> bool {
    headers
        .get_all(http::header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| value.contains("text/html"))
}

/// Turn a URL-relative request path into a safe relative filesystem path.
///
/// Each `/`-separated segment is percent-decoded (so filenames containing
/// `%20`, `%2B`, etc. resolve correctly and consistently with the agent/session
/// routes). Decoding is per-segment so an encoded separator (`%2F`) cannot
/// smuggle an extra path boundary. Returns `None` if the path attempts to
/// traverse out of the assets root (a `..` component or an absolute prefix
/// survives normalization). Current-dir and root components are skipped so
/// leading slashes are harmless.
fn sanitize_asset_path(path: &str) -> Option<PathBuf> {
    let decoded = path
        .split('/')
        .map(percent_decode)
        .collect::<Vec<_>>()
        .join("/");
    let mut sanitized = PathBuf::new();
    for component in Path::new(&decoded).components() {
        match component {
            Component::Normal(segment) => sanitized.push(segment),
            Component::CurDir | Component::RootDir => continue,
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    Some(sanitized)
}

/// Whether the request path names a concrete file (has an extension). Used to
/// decide if the SPA index.html fallback should apply.
fn has_file_extension(path: &Path) -> bool {
    path.extension().is_some()
}

/// Whether the client accepts HTML (a navigation request).
fn wants_html(headers: &http::HeaderMap) -> bool {
    headers
        .get_all(http::header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| {
            value
                .split(',')
                .any(|item| item.trim().starts_with("text/html"))
        })
}

/// Best-effort Content-Type from a file extension. Unknown extensions default
/// to `application/octet-stream`.
fn content_type_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("wasm") => "application/wasm",
        Some("map") => "application/json; charset=utf-8",
        Some("txt") => "text/plain; charset=utf-8",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn accepts_event_stream(headers: &http::HeaderMap) -> bool {
    headers
        .get_all(http::header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(accept_header_allows_event_stream)
}

fn accept_header_allows_event_stream(value: &str) -> bool {
    value.split(',').any(|raw_item| {
        let mut parts = raw_item.split(';');
        let media_type = parts.next().unwrap_or_default().trim();
        if !media_type.eq_ignore_ascii_case("text/event-stream") {
            return false;
        }

        for param in parts {
            let mut kv = param.splitn(2, '=');
            let name = kv.next().unwrap_or_default().trim();
            let value = kv.next().unwrap_or_default().trim().trim_matches('"');
            if name.eq_ignore_ascii_case("q") {
                match value.parse::<f32>() {
                    Ok(q) if q <= 0.0 => return false,
                    Ok(_) => {}
                    Err(_) => return false,
                }
            }
        }

        true
    })
}

/// Ordering for the web session list: most-recently-modified first so active
/// work surfaces at the top. Sessions without a modified time sort last; ties
/// (equal or both-missing modified) fall back to id (descending) for stable,
/// deterministic ordering.
pub(crate) fn session_recency_ordering(
    left: &harnx_runtime::config::SessionMeta,
    right: &harnx_runtime::config::SessionMeta,
) -> std::cmp::Ordering {
    right
        .modified
        .cmp(&left.modified)
        .then_with(|| right.id.cmp(&left.id))
}

/// Derive repository and branch from execution contexts.
/// Mirrors TUI picker_label logic: pick the first context with repo or branch,
/// then take each field independently.
pub(crate) fn session_repository_and_branch(
    contexts: &[harnx_core::execution_context::ExecutionContextObservation],
) -> (Option<String>, Option<String>) {
    let matched_context = contexts
        .iter()
        .find(|ctx| ctx.primary_repository().is_some() || ctx.branch().is_some());
    (
        matched_context
            .and_then(|ctx| ctx.primary_repository())
            .map(|s| s.to_owned()),
        matched_context
            .and_then(|ctx| ctx.branch())
            .map(|s| s.to_owned()),
    )
}

pub(crate) fn format_session_summary(session: &harnx_runtime::config::SessionMeta) -> Value {
    let session_id = session.session_id.as_deref().unwrap_or(&session.id);
    let mut value = serde_json::Map::from_iter([(
        String::from("session_id"),
        Value::String(session_id.to_string()),
    )]);
    value.insert(
        String::from("title"),
        session
            .title
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    if let Some(modified) = session.modified {
        value.insert(
            String::from("updated_at"),
            Value::String(format_system_time(modified)),
        );
    }
    value.insert(String::from("unread"), Value::Bool(session.unread));
    value.insert(
        String::from("user_id"),
        session
            .user_id
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );

    // Derive repository and branch from contexts, mirroring TUI picker_label logic:
    // pick the first context with repo or branch, then take each field independently.
    let (repository, branch) = session_repository_and_branch(&session.contexts);
    value.insert(
        String::from("repository"),
        repository.map(Value::String).unwrap_or(Value::Null),
    );
    value.insert(
        String::from("branch"),
        branch.map(Value::String).unwrap_or(Value::Null),
    );

    Value::Object(value)
}

async fn agent_sessions_json(
    config: &Config,
    target: &ResolvedAgentTarget,
    access: Option<(&AccessRules, CallerView<'_>)>,
) -> Result<Vec<Value>> {
    let sessions = list_target_sessions(config, target, access).await?;
    Ok(sessions.iter().map(format_session_summary).collect())
}

/// Load session history from NATS durable log.
/// Returns tuple of (Session, entries) so callers can access raw log entries
/// for control-state hydration on promptless attach.
pub(crate) async fn load_nats_session(
    config: &Config,
    target: &ResolvedAgentTarget,
    session: &str,
) -> Result<(
    harnx_core::session::Session,
    Vec<(u64, harnx_core::session::SessionLogEntry)>,
)> {
    let (loaded, entries, _base_session) =
        load_nats_session_with_base(config, target, session).await?;
    Ok((loaded, entries))
}

/// Load session history and retain canonical metadata state for replaying a fresher log snapshot.
pub(crate) async fn load_nats_session_with_base(
    config: &Config,
    target: &ResolvedAgentTarget,
    session: &str,
) -> Result<(
    harnx_core::session::Session,
    Vec<(u64, harnx_core::session::SessionLogEntry)>,
    harnx_core::session::Session,
)> {
    let loaded = load_nats_session_state(config, target, session).await?;
    Ok((loaded.session, loaded.entries, loaded.base_session))
}

/// One durable read of a session: its replayed history, the raw log entries and
/// whether a worker held its lease across the read. The lease sample is taken
/// here because this read needs it anyway, and sampling it again would mean a
/// second connection for an answer this one already has.
pub(crate) struct LoadedNatsSession {
    pub(crate) session: harnx_core::session::Session,
    pub(crate) entries: Vec<(u64, harnx_core::session::SessionLogEntry)>,
    pub(crate) base_session: harnx_core::session::Session,
    pub(crate) worker_active: bool,
    /// When the log's unanswered prompt was admitted, when that can be
    /// established. Stall detection ages the prompt from it; `None` on paths
    /// that had no admission to read or when the record is unavailable.
    pub(crate) pending_prompt_admitted_at: Option<DateTime<Utc>>,
    /// The read's `pending_prompt_seq` answer, resolved once so state
    /// derivation and the admission lookup share it.
    pub(crate) pending_prompt_seq: Option<u64>,
}

pub(crate) async fn load_nats_session_state(
    config: &Config,
    target: &ResolvedAgentTarget,
    session: &str,
) -> Result<LoadedNatsSession> {
    ensure_frontend_nats_owner(target.cluster()).await?;
    let jetstream = serve_nats_jetstream(config, target.cluster()).await?;
    let metadata_store =
        harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1).await?;
    let storage_key = harnx_core::session_identity::session_key(Some(target.agent()), session);
    let metadata = metadata_store
        .get(&storage_key)
        .await?
        .ok_or_else(|| anyhow!("Not Found"))?
        .metadata;
    // Double-sample lease status: before and after the bounded log read.
    // Mirrors load_remote_transcript_for_render in harnx-runtime. A trailing
    // ToolCalls row is in flight (pending) rather than interrupted when either
    // sample shows an active lease.
    let lease_was_active = worker_lease_sample(&jetstream, &storage_key, session).await?;
    let log = harnx_runtime::nats_session_log::NatsSessionLog::new(
        jetstream.clone(),
        storage_key.clone(),
    );
    let entries = log
        .load_events_async()
        .await
        .map_err(|err| anyhow!("Failed to load session history for '{session}': {err}"))?;
    reconcile_attention(&metadata_store, &storage_key, session, &entries).await;
    // If there was no holder before the read, sample once more to cover a
    // worker acquiring the lease and appending ToolCalls during the read.
    let worker_active = match lease_was_active {
        true => true,
        false => worker_lease_sample(&jetstream, &storage_key, session).await?,
    };
    // A worker holding the lease makes a stall impossible, so only a
    // workerless read pays for the prompt's admission age. The prompt sequence
    // itself is resolved once here for every read — leased ones included —
    // because the state derivation that consumes it distinguishes
    // Pending/Idle/Interrupted on leased sessions too. This is that
    // derivation's one replay of the log's mutations, not an extra one.
    let prompt_seq = harnx_core::session_reconstruct::pending_prompt_seq(&entries);
    let pending_prompt_admitted_at = if worker_active {
        None
    } else {
        pending_prompt_admitted_at(&metadata_store, &storage_key, prompt_seq, &entries).await
    };
    let base_session = metadata.base_session();
    let loaded = if worker_active {
        harnx_runtime::nats_session_log::load_session_from_entries_with_metadata_preserving_pending(
            &entries,
            session,
            base_session.clone(),
        )
    } else {
        harnx_runtime::nats_session_log::load_session_from_entries_with_metadata(
            &entries,
            session,
            base_session.clone(),
        )
    }
    .map_err(|err| anyhow!("Failed to reconstruct session history for '{session}': {err}"))?;
    Ok(LoadedNatsSession {
        session: loaded,
        entries,
        base_session,
        worker_active,
        pending_prompt_admitted_at,
        pending_prompt_seq: prompt_seq,
    })
}

/// When the log's newest unanswered prompt was admitted. Best effort: the
/// prompt's own timestamp when the durable row carries one, else the
/// `admitted_at` of its admission record. A prompt whose age cannot be
/// established reads as running rather than stalled. `prompt_seq` is the
/// read's resolved `pending_prompt_seq`; when an edit replaced the prompt,
/// the age is traced back to the original prompt row it rewrote.
async fn pending_prompt_admitted_at(
    metadata_store: &harnx_runtime::nats_session_metadata::SessionMetadataStore,
    storage_key: &str,
    prompt_seq: Option<u64>,
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
) -> Option<DateTime<Utc>> {
    let prompt_seq = prompt_seq?;
    let entry = prompt_age_row(entries, prompt_seq)?;
    if let Some(timestamp) = entry.timestamp() {
        return Some(timestamp);
    }
    let SessionLogEntry::Message {
        id: Some(prompt_id),
        ..
    } = entry
    else {
        return None;
    };
    match metadata_store
        .prompt_admission(storage_key, prompt_id)
        .await
    {
        Ok(Some(admission)) => Some(admission.admitted_at),
        Ok(None) => None,
        Err(error) => {
            // The prompt's age is what makes a stalled turn visible; a store
            // that answers nothing silently regresses the read to reporting
            // running forever, so surface the failure.
            log::warn!(
                "session read: prompt admission lookup failed for '{storage_key}' prompt '{prompt_id}': {error:#}"
            );
            None
        }
    }
}

/// The row whose admission dates the pending prompt: the prompt itself, or —
/// when an edit replaced it — the original prompt row the `EditEntries` chain
/// rewrote. A replacement carries a fresh client id and no timestamp
/// (`edit_user_message`), so neither its own admission lookup nor its row
/// time can date the turn; the original's admission is the claim deadline
/// that was already running for it.
fn prompt_age_row(entries: &[(u64, SessionLogEntry)], prompt_seq: u64) -> Option<&SessionLogEntry> {
    let (_, entry) = entries.iter().find(|(seq, _)| *seq == prompt_seq)?;
    match entry {
        SessionLogEntry::EditEntries { from, to, .. } => original_prompt_row(entries, *from, *to),
        _ => Some(entry),
    }
}

/// The original prompt row an `EditEntries` range rewrote: the newest user
/// message inside the range, or — when the range replaced an earlier edit's
/// mutation row instead of a prompt (chained edits) — the row that nested
/// edit rewrote. The trace is depth-capped: a crafted log whose ranges point
/// at each other would otherwise recurse forever, and a bounded miss reads as
/// running — the same answer an untraceable age already gets.
const MAX_EDIT_TRACE_DEPTH: usize = 32;

fn original_prompt_row(
    entries: &[(u64, SessionLogEntry)],
    from: usize,
    to: usize,
) -> Option<&SessionLogEntry> {
    trace_edit_chain(entries, from, to, MAX_EDIT_TRACE_DEPTH)
}

fn trace_edit_chain(
    entries: &[(u64, SessionLogEntry)],
    from: usize,
    to: usize,
    depth: usize,
) -> Option<&SessionLogEntry> {
    if depth == 0 {
        return None;
    }
    let is_user_prompt = |entry: &SessionLogEntry| matches!(entry, SessionLogEntry::Message { id: Some(_), role, .. } if role.is_user());
    if let Some((_, entry)) = newest_row_in_range(entries, from, to, is_user_prompt) {
        return Some(entry);
    }
    // Chained edit: the row this edit replaced is an earlier edit's mutation.
    let nested_edit = newest_row_in_range(entries, from, to, |entry| {
        matches!(entry, SessionLogEntry::EditEntries { .. })
    })?;
    match nested_edit {
        (_, SessionLogEntry::EditEntries { from, to, .. }) => {
            trace_edit_chain(entries, *from, *to, depth - 1)
        }
        _ => None,
    }
}

fn newest_row_in_range(
    entries: &[(u64, SessionLogEntry)],
    from: usize,
    to: usize,
    is_match: impl Fn(&SessionLogEntry) -> bool,
) -> Option<&(u64, SessionLogEntry)> {
    let in_range = |row: &(u64, SessionLogEntry)| {
        usize::try_from(row.0).is_ok_and(|seq| seq >= from && seq <= to)
    };
    entries
        .iter()
        .rev()
        .find(|row| in_range(row) && is_match(&row.1))
}

/// Whether a worker holds this session's lease right now.
async fn worker_lease_sample(
    jetstream: &harnx_runtime::nats_event_sink::JetstreamContext,
    storage_key: &str,
    session: &str,
) -> Result<bool> {
    harnx_runtime::nats_lease::session_has_active_lease(jetstream, storage_key)
        .await
        .map_err(|err| anyhow!("Failed to inspect worker lease for session '{session}': {err}"))
}

/// Repair attention state from the log on the server read path: a bump lost on
/// the way in would leave a session looking read when it is not. Best effort —
/// a session's history is worth returning even when its unread mark is stale.
async fn reconcile_attention(
    metadata_store: &harnx_runtime::nats_session_metadata::SessionMetadataStore,
    storage_key: &str,
    session: &str,
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
) {
    let attention_seq = harnx_runtime::nats_worker::derive_attention_seq(entries);
    if attention_seq == 0 {
        return;
    }
    if let Err(error) = metadata_store
        .bump_attention(storage_key, attention_seq)
        .await
    {
        log::warn!(
            "failed to reconcile attention on server read: session_id={session} storage_key={storage_key} seq={attention_seq} error={error:#}"
        );
    }
}

/// Test helper exposing load_nats_session_with_base for integration tests.
#[doc(hidden)]
pub async fn load_nats_session_with_base_for_test(
    config: &Config,
    agent_ref: &str,
    session: &str,
) -> Result<(
    harnx_core::session::Session,
    Vec<(u64, harnx_core::session::SessionLogEntry)>,
    harnx_core::session::Session,
)> {
    let (target, _) = resolve_agent_target(config, agent_ref)
        .await
        .map_err(ag_ui_error_to_anyhow)?;
    load_nats_session_with_base(config, &target, session).await
}

#[doc(hidden)]
pub async fn ensure_frontend_nats_owner(cluster: &str) -> Result<()> {
    if cluster != LOCAL_CLUSTER_KEY {
        return Ok(());
    }
    let key = harnx_core::config_paths::nats_runtime_ports_file();
    let mut handles = LOCAL_NATS_HANDLES.lock().await;
    if !handles.contains_key(&key) {
        let server = harnx_runtime::nats_local_server::ensure_shared_server().await?;
        handles.insert(key.clone(), server);
    }
    Ok(())
}

fn format_system_time(value: SystemTime) -> String {
    // Fixed millisecond precision so every emitted `updated_at` is uniform length.
    // The session list itself is sorted on the underlying `SystemTime` (not the
    // string), so ordering is always chronologically correct; uniform precision
    // just keeps the serialized values consistent for any downstream consumer.
    DateTime::<Utc>::from(value).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn history_role_name(role: MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::Assistant => "assistant",
        MessageRole::User => "user",
        MessageRole::Tool => "tool",
    }
}

fn history_message_content(message: &Message) -> String {
    match &message.content {
        harnx_core::message::MessageContent::ToolCalls(tool_calls) => {
            if let Some(thought) = tool_calls
                .thought
                .as_deref()
                .filter(|thought| !thought.is_empty())
            {
                if tool_calls.text.is_empty() {
                    thought.to_string()
                } else {
                    format!("<think>\n{thought}\n</think>\n{}", tool_calls.text)
                }
            } else {
                tool_calls.text.clone()
            }
        }
        _ => message.content.to_transcript_text(),
    }
}

fn history_message_id(message: &Message, seq_counts: &mut BTreeMap<usize, usize>) -> String {
    if let Some(id) = &message.id {
        return id.clone();
    }

    match message.log_seq {
        Some(seq) => {
            let subindex = seq_counts.entry(seq).or_insert(0);
            let id = format!("seq:{seq}:{subindex}");
            *subindex += 1;
            id
        }
        None => {
            let subindex = seq_counts.entry(usize::MAX).or_insert(0);
            let id = format!("seq:none:{subindex}");
            *subindex += 1;
            id
        }
    }
}

fn content_type_is_json(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            // Case-insensitive media-type match; tolerate parameters like
            // `application/json; charset=utf-8`.
            let media_type = value.split(';').next().unwrap_or_default().trim();
            media_type.eq_ignore_ascii_case("application/json")
        })
}

fn json_response(data: Value) -> Result<AppResponse> {
    let res = Response::builder()
        .header("Content-Type", "application/json; charset=utf-8")
        .body(Full::new(Bytes::from(data.to_string())).boxed())?;
    Ok(res)
}

fn json_response_with_status(status: StatusCode, data: Value) -> Result<AppResponse> {
    let res = Response::builder()
        .status(status)
        .header("Content-Type", "application/json; charset=utf-8")
        .body(Full::new(Bytes::from(data.to_string())).boxed())?;
    Ok(res)
}

fn attachment_error_response(status: StatusCode, message: &str) -> Result<AppResponse> {
    let response = Response::builder()
        .status(status)
        .header("Content-Type", "application/json; charset=utf-8")
        .header("X-Content-Type-Options", "nosniff")
        .header(http::header::CACHE_CONTROL, "private, no-store")
        .body(Full::new(Bytes::from(json!({ "error": message }).to_string())).boxed())?;
    Ok(response)
}

fn ag_ui_error_to_anyhow(err: AgUiError) -> anyhow::Error {
    let status = err.status_code();
    let code = err.error_code();
    annotate_error_message(err, status, code)
}

fn status_from_error(err: &anyhow::Error) -> Option<StatusCode> {
    let message = err.to_string();
    if message == "Not Found" {
        return Some(StatusCode::NOT_FOUND);
    }
    if message == "Method Not Allowed" {
        return Some(StatusCode::METHOD_NOT_ALLOWED);
    }
    if message == "Not Acceptable" {
        return Some(StatusCode::NOT_ACCEPTABLE);
    }
    // Only a server-authored annotation supplies a status: raw text is never
    // parsed for markers, so caller-controlled strings cannot spoof one (#2955).
    // qual:allow(coupling, deh) reason: "the downcast is the trusted provenance boundary: only a typed AnnotatedError frame supplies a status, so raw text can never be parsed as an annotation (#2955)."
    err.downcast_ref::<AnnotatedError>().map(|a| a.status)
}

/// A guard failure otherwise surfaces as the generic 400: the error reaches
/// `finalize_err_status` with no status of its own. Broker-unavailability
/// failures get the 503 the guarded route would answer itself; guard
/// denials keep the 404 their message already carries. The annotation is a
/// typed context wrapping the original error, so the chain the dispatch log
/// prints stays intact and log redaction sees the original text.
pub(crate) fn guard_error_status(err: anyhow::Error) -> anyhow::Error {
    if crate::nats_access::is_nats_unavailable(&err) {
        let context = annotated(&err, StatusCode::SERVICE_UNAVAILABLE, None);
        return err.context(context);
    }
    err
}

/// Final HTTP status for a failed dispatch. An explicit dispatch status wins;
/// otherwise the error mapping applies, with bad request as the fallback.
fn finalize_err_status(dispatch_status: StatusCode, err: &anyhow::Error) -> StatusCode {
    if dispatch_status == StatusCode::OK {
        status_from_error(err).unwrap_or(StatusCode::BAD_REQUEST)
    } else {
        dispatch_status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models_catalog::DEFAULT_MODEL_NAME;
    use crate::test_support::{seed_nats_session, NatsSessionSeed, TestConfigSandbox};
    use harnx_core::message::{ImageUrl, Message, MessageContent, MessageContentPart};
    use harnx_core::session::SessionLogEntry;
    use http::HeaderValue;
    use tokio::{io::AsyncWriteExt, net::TcpStream};

    fn agents_request(query: Option<&str>) -> hyper::Request<()> {
        let uri = query.map_or_else(|| "/v1/agents".to_string(), |q| format!("/v1/agents?{q}"));
        hyper::Request::builder().uri(uri).body(()).unwrap()
    }

    #[test]
    fn web_assets_warning_reports_missing_or_incomplete_directories() {
        let tempdir = tempfile::tempdir().expect("tempdir");

        let missing = tempdir.path().join("missing");
        let warning = web_assets_warning(&missing).expect("missing directory should warn");
        assert!(warning.contains(&missing.display().to_string()));

        let empty = tempdir.path().join("empty");
        std::fs::create_dir(&empty).expect("create empty assets directory");
        let warning = web_assets_warning(&empty).expect("directory without index.html should warn");
        assert!(warning.contains(&empty.display().to_string()));

        std::fs::write(empty.join("index.html"), "<html></html>").expect("write index.html");
        assert_eq!(web_assets_warning(&empty), None);
    }

    #[test]
    fn browser_base_url_maps_wildcards_and_brackets_ipv6() {
        // Wildcard IPv4 -> loopback so the URL is clickable.
        assert_eq!(
            browser_base_url("0.0.0.0:8000".parse().unwrap()),
            "http://127.0.0.1:8000"
        );
        // Wildcard IPv6 -> loopback, bracketed.
        assert_eq!(
            browser_base_url("[::]:8000".parse().unwrap()),
            "http://[::1]:8000"
        );
        // Concrete IPv4 passes through with its real port.
        assert_eq!(
            browser_base_url("127.0.0.1:9123".parse().unwrap()),
            "http://127.0.0.1:9123"
        );
        // Concrete IPv6 loopback stays bracketed.
        assert_eq!(
            browser_base_url("[::1]:8000".parse().unwrap()),
            "http://[::1]:8000"
        );
        // Concrete global IPv6 stays bracketed.
        assert_eq!(
            browser_base_url("[2001:db8::1]:8080".parse().unwrap()),
            "http://[2001:db8::1]:8080"
        );
        // Ephemeral (0) and max ports are preserved verbatim.
        assert_eq!(
            browser_base_url("127.0.0.1:0".parse().unwrap()),
            "http://127.0.0.1:0"
        );
        assert_eq!(
            browser_base_url("0.0.0.0:65535".parse().unwrap()),
            "http://127.0.0.1:65535"
        );
        // IPv4-mapped IPv6 wildcard normalizes to plain IPv4 loopback.
        assert_eq!(
            browser_base_url("[::ffff:0.0.0.0]:8000".parse().unwrap()),
            "http://127.0.0.1:8000"
        );
        // IPv4-mapped IPv6 concrete address normalizes to plain IPv4.
        assert_eq!(
            browser_base_url("[::ffff:127.0.0.1]:8000".parse().unwrap()),
            "http://127.0.0.1:8000"
        );
        // Link-local IPv6 preserves its zone/scope id, RFC 6874 `%25`-escaped.
        let scoped = SocketAddr::V6(std::net::SocketAddrV6::new(
            "fe80::1".parse().unwrap(),
            8000,
            0,
            18,
        ));
        assert_eq!(browser_base_url(scoped), "http://[fe80::1%2518]:8000");
    }

    #[test]
    fn advertised_base_url_prefers_local_addr_and_falls_back_to_requested() {
        // With a resolved local address, use it (wildcard maps to loopback,
        // ephemeral port resolved to the real one).
        assert_eq!(
            advertised_base_url(Some("0.0.0.0:45877".parse().unwrap()), "0.0.0.0:0"),
            "http://127.0.0.1:45877"
        );
        // Without a local address, fall back to the requested address string.
        assert_eq!(
            advertised_base_url(None, "127.0.0.1:8000"),
            "http://127.0.0.1:8000"
        );
        // Fallback preserves whatever the caller passed, even a bare host:port.
        assert_eq!(
            advertised_base_url(None, "example.internal:9000"),
            "http://example.internal:9000"
        );
    }

    async fn server_with_incomplete_request(
        drain_timeout: Duration,
    ) -> Result<(ServerHandle, TcpStream)> {
        let server = Arc::new(asset_server(PathBuf::from("web-assets")));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server_handle = server.run(listener, drain_timeout).await?;
        let mut stream = TcpStream::connect(address).await?;
        stream
            .write_all(
                b"POST /v1/embeddings HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{",
            )
            .await?;
        tokio::time::sleep(Duration::from_millis(25)).await;
        Ok((server_handle, stream))
    }

    #[tokio::test]
    async fn shutdown_waits_for_in_flight_http_request() -> Result<()> {
        let (server_handle, mut stream) =
            server_with_incomplete_request(Duration::from_secs(1)).await?;
        let mut shutdown = Box::pin(server_handle.shutdown());

        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut shutdown)
                .await
                .is_err(),
            "shutdown returned before the in-flight request completed"
        );

        stream.write_all(b"}").await?;
        tokio::time::timeout(Duration::from_secs(1), shutdown)
            .await
            .context("server did not finish after the request completed")??;
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_aborts_connections_at_drain_deadline() -> Result<()> {
        let (server_handle, _stream) =
            server_with_incomplete_request(Duration::from_millis(25)).await?;

        tokio::time::timeout(Duration::from_secs(1), server_handle.shutdown())
            .await
            .context("server exceeded its configured drain deadline")??;
        Ok(())
    }

    #[test]
    fn percent_decode_decodes_slashes_and_preserves_invalid_escapes() {
        assert_eq!(percent_decode("coding%2Fcoder"), "coding/coder");
        assert_eq!(percent_decode("sisyphus%40shared"), "sisyphus@shared");
        assert_eq!(
            percent_decode("coding%2Fcoder%40shared"),
            "coding/coder@shared"
        );
        assert_eq!(percent_decode("hephaestus"), "hephaestus");
        assert_eq!(percent_decode("bad%2"), "bad%");
        assert_eq!(percent_decode("bad%zz"), "bad%");
    }

    #[test]
    fn query_requests_assistant_role_only_matches_explicit_assistant_filter() {
        assert!(query_requests_assistant_role(Some("role=assistant")));
        assert!(query_requests_assistant_role(Some(
            "foo=bar&role=assistant"
        )));
        assert!(!query_requests_assistant_role(None));
        assert!(!query_requests_assistant_role(Some("role=subagent")));
        assert!(!query_requests_assistant_role(Some("assistants=true")));
    }

    #[test]
    fn parse_agents_route_matches_history_with_decoded_agent_and_session() {
        for (path, agent, session) in [
            (
                "/v1/agents/plain/sessions/my-session/history",
                "plain",
                "my-session",
            ),
            (
                "/v1/agents/coding%2Fcoder/sessions/thread%2F1/history",
                "coding/coder",
                "thread/1",
            ),
        ] {
            assert_eq!(
                parse_agents_route(path),
                Some((
                    agent.to_string(),
                    Some(session.to_string()),
                    AgentsRoute::SessionHistory,
                ))
            );
        }
    }

    #[test]
    fn parse_agents_route_extracts_and_decodes_agent_and_session_segments_and_leaves_keywords_literal(
    ) {
        assert_eq!(
            parse_agents_route("/v1/agents/hephaestus"),
            Some(("hephaestus".to_string(), None, AgentsRoute::Agent))
        );
        assert_eq!(
            parse_agents_route("/v1/agents/coding%2Fcoder"),
            Some(("coding/coder".to_string(), None, AgentsRoute::Agent))
        );
        for (path, expected_agent) in [
            ("/v1/agents/sisyphus%40shared", "sisyphus"),
            ("/v1/agents/coding%2Fcoder%40shared", "coding/coder"),
        ] {
            let (decoded, session, route) = parse_agents_route(path).expect("agent route");
            assert_eq!(session, None);
            assert_eq!(route, AgentsRoute::Agent);
            assert_eq!(
                AgentRef::parse(&decoded),
                AgentRef::Remote {
                    agent: expected_agent.into(),
                    cluster: "shared".into(),
                }
            );
        }
        assert_eq!(
            parse_agents_route("/v1/agents/coding%2Fcoder/sessions"),
            Some(("coding/coder".to_string(), None, AgentsRoute::Sessions))
        );
        assert_eq!(
            parse_agents_route("/v1/agents/coding%2Fcoder/sessions/thread%2F1"),
            Some((
                "coding/coder".to_string(),
                Some("thread/1".to_string()),
                AgentsRoute::Session,
            ))
        );
        assert_eq!(
            parse_agents_route("/v1/agents/coding%2Fcoder/sessions/thread-1/events"),
            Some((
                "coding/coder".to_string(),
                Some("thread-1".to_string()),
                AgentsRoute::SessionEvents,
            ))
        );
        assert_eq!(parse_agents_route("/v1/agents"), None);
        assert_eq!(parse_agents_route("/v1/agents/hephaestus/extra"), None);
        assert_eq!(
            parse_agents_route("/v1/agents/hephaestus%2Fsessions"),
            Some(("hephaestus/sessions".to_string(), None, AgentsRoute::Agent,))
        );
    }

    #[test]
    fn session_attachments_path_matches_only_attachment_route() {
        assert!(is_session_attachments_path(
            "/v1/agents/hephaestus/sessions/thread-1/attachments"
        ));
        assert_eq!(
            parse_session_attachments_path("/v1/agents/hephaestus/sessions/thread-1/attachments"),
            Some(("hephaestus".to_string(), "thread-1".to_string()))
        );
        assert_eq!(
            parse_session_attachments_path(
                "/v1/agents/coding%2Fcoder/sessions/thread-1/attachments"
            ),
            Some(("coding/coder".to_string(), "thread-1".to_string()))
        );
        assert!(!is_session_attachments_path("/v1/agents/hephaestus/rpc"));
        assert!(!is_session_attachments_path(
            "/v1/agents/hephaestus/sessions/thread-1"
        ));
        assert!(parse_session_attachments_path(
            "/v1/agents/hephaestus/sessions/%2E%2E/attachments"
        )
        .is_none());
        assert!(parse_session_attachments_path(
            "/v1/agents/hephaestus/sessions/%2E%2E%2Foutside/attachments"
        )
        .is_none());
    }

    #[test]
    fn session_attachment_blob_path_decodes_and_validates_cid_segment() {
        let hash = "a".repeat(64);
        let cid = format!("cid:media:coding%2Fcoder/thread-1/{hash}");
        let encoded_path = format!(
            "/v1/agents/coding%2Fcoder/sessions/thread-1/attachments/cid%3Amedia%3Acoding%252Fcoder%2Fthread-1%2F{hash}"
        );
        assert!(is_session_attachment_blob_path(&encoded_path));
        assert_eq!(
            parse_session_attachment_blob_path(&encoded_path),
            Some((
                "coding/coder".to_string(),
                "thread-1".to_string(),
                cid.clone(),
            ))
        );
        assert!(is_canonical_attachment_cid(cid.as_bytes()));
        assert!(!is_canonical_attachment_cid(
            format!("cid:{hash}").as_bytes()
        ));
        assert!(!is_canonical_attachment_cid(
            format!("cid:media:coding%2Fcoder/thread-1/{}", "A".repeat(64)).as_bytes()
        ));
        assert!(!is_canonical_attachment_cid(
            b"cid:plan:coding%2Fcoder/thread-1/my-plan"
        ));
        assert!(!is_canonical_attachment_cid(b"cid:../outside"));
        assert!(!is_session_attachment_blob_path(
            "/v1/agents/hephaestus/sessions/thread-1/attachments"
        ));
    }
    #[test]
    fn negotiate_agents_route_prefers_html_for_browser_gets() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("text/html,application/xhtml+xml"),
        );
        assert_eq!(
            negotiate_agents_route(&Method::GET, &headers, AgentsRoute::Agent).unwrap(),
            AgentsRepresentation::Html
        );
        assert_eq!(
            negotiate_agents_route(&Method::GET, &headers, AgentsRoute::Session).unwrap(),
            AgentsRepresentation::Html
        );
    }

    #[test]
    fn negotiate_agents_route_defaults_gets_to_json() {
        let headers = http::HeaderMap::new();
        assert_eq!(
            negotiate_agents_route(&Method::GET, &headers, AgentsRoute::Agent).unwrap(),
            AgentsRepresentation::Json
        );
        assert_eq!(
            negotiate_agents_route(&Method::GET, &headers, AgentsRoute::Sessions).unwrap(),
            AgentsRepresentation::Json
        );
        assert_eq!(
            negotiate_agents_route(&Method::GET, &headers, AgentsRoute::Session).unwrap(),
            AgentsRepresentation::Json
        );
    }

    #[test]
    fn negotiate_agents_route_accepts_session_collection_post_for_reservation() {
        assert_eq!(
            negotiate_agents_route(
                &Method::POST,
                &http::HeaderMap::new(),
                AgentsRoute::Sessions,
            )
            .unwrap(),
            AgentsRepresentation::Json
        );
    }

    #[test]
    fn negotiate_agents_route_accepts_session_event_feed() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        assert_eq!(
            negotiate_agents_route(&Method::GET, &headers, AgentsRoute::SessionEvents).unwrap(),
            AgentsRepresentation::AgUiSse
        );
        assert_eq!(
            negotiate_agents_route(
                &Method::GET,
                &http::HeaderMap::new(),
                AgentsRoute::SessionEvents,
            )
            .unwrap_err()
            .to_string(),
            "Method Not Allowed"
        );
    }

    #[test]
    fn negotiate_agents_route_accepts_repeated_accept_fields() {
        // Repeated Accept fields and a comma-joined field have the same meaning.
        for accept in ["text/html", "text/event-stream;q=0.5"] {
            let mut split = http::HeaderMap::new();
            split.append(
                http::header::ACCEPT,
                HeaderValue::from_static("application/json"),
            );
            split.append(http::header::ACCEPT, HeaderValue::from_static(accept));
            split.insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            let mut joined = split.clone();
            joined.insert(
                http::header::ACCEPT,
                format!("application/json, {accept}").parse().unwrap(),
            );
            for (method, route) in [
                (Method::GET, AgentsRoute::Agent),
                (Method::GET, AgentsRoute::Session),
                (Method::POST, AgentsRoute::Session),
                (Method::GET, AgentsRoute::SessionEvents),
            ] {
                let negotiate = |headers| {
                    negotiate_agents_route(&method, headers, route)
                        .map_err(|error| error.to_string())
                };
                assert_eq!(
                    negotiate(&split),
                    negotiate(&joined),
                    "{method} {route:?} {accept}"
                );
            }
            assert_eq!(wants_html(&split), wants_html(&joined));
        }
    }

    #[test]
    fn repeated_accept_fields_preserve_event_stream_quality_checks() {
        let mut headers = http::HeaderMap::new();
        headers.append(
            http::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        headers.append(
            http::header::ACCEPT,
            HeaderValue::from_static("text/event-stream;q=0"),
        );
        assert!(!accepts_event_stream(&headers));
        headers.append(
            http::header::ACCEPT,
            HeaderValue::from_bytes(&[0xff]).unwrap(),
        );
        assert!(!accepts_event_stream(&headers));
        headers.append(
            http::header::ACCEPT,
            HeaderValue::from_static("text/event-stream;q=0.5"),
        );
        assert!(accepts_event_stream(&headers));
    }

    #[test]
    fn negotiate_agents_route_accepts_ag_ui_post_shape() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        assert_eq!(
            negotiate_agents_route(&Method::POST, &headers, AgentsRoute::Session).unwrap(),
            AgentsRepresentation::AgUiSse
        );
    }

    #[test]
    fn negotiate_agents_route_rejects_wrong_method_and_headers() {
        let headers = http::HeaderMap::new();
        assert_eq!(
            negotiate_agents_route(&Method::POST, &headers, AgentsRoute::Session)
                .unwrap_err()
                .to_string(),
            "Not Acceptable"
        );
        assert_eq!(
            negotiate_agents_route(&Method::DELETE, &headers, AgentsRoute::Agent)
                .unwrap_err()
                .to_string(),
            "Method Not Allowed"
        );
    }

    #[tokio::test]
    async fn agent_route_error_mapping_covers_404_405_and_406_shapes() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();

        let missing_agent = resolve_agent_target(&config, "missing")
            .await
            .map_err(ag_ui_error_to_anyhow);
        let missing_err = missing_agent.expect_err("unknown agent should 404");
        assert_eq!(missing_err.to_string(), "agent 'missing' not found");
        assert_eq!(status_from_error(&missing_err), Some(StatusCode::NOT_FOUND));
        let missing_response = response_json(dispatch_error_response(&missing_err)).await;
        assert_eq!(
            missing_response["error"]["message"],
            "agent 'missing' not found"
        );
        assert_eq!(
            missing_response["error"]["code"], "agent_not_found",
            "agent-not-found body must carry the stable code"
        );
        assert!(!missing_response.to_string().contains("__status"));
        assert!(!missing_response.to_string().contains("__code"));

        let unknown_cluster = resolve_agent_target(&config, "atlas@missing-cluster")
            .await
            .expect_err("unknown cluster should fail");
        assert!(matches!(unknown_cluster, AgUiError::BadRequest(_)));
        assert!(unknown_cluster.to_string().contains("missing-cluster"));
        let unknown_cluster = ag_ui_error_to_anyhow(unknown_cluster);
        assert_eq!(
            status_from_error(&unknown_cluster),
            Some(StatusCode::BAD_REQUEST)
        );

        let wrong_method_err =
            negotiate_agents_route(&Method::DELETE, &http::HeaderMap::new(), AgentsRoute::Agent)
                .expect_err("wrong method should 405");
        assert_eq!(wrong_method_err.to_string(), "Method Not Allowed");
        assert_eq!(
            status_from_error(&wrong_method_err),
            Some(StatusCode::METHOD_NOT_ALLOWED)
        );

        let mut bad_post_headers = http::HeaderMap::new();
        bad_post_headers.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        bad_post_headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain"),
        );
        let not_acceptable_err =
            negotiate_agents_route(&Method::POST, &bad_post_headers, AgentsRoute::Session)
                .expect_err("bad post accept should 406");
        assert_eq!(not_acceptable_err.to_string(), "Not Acceptable");
        assert_eq!(
            status_from_error(&not_acceptable_err),
            Some(StatusCode::NOT_ACCEPTABLE)
        );

        let conflict = ag_ui_error_to_anyhow(AgUiError::Conflict("test rejection".into()));
        assert_eq!(status_from_error(&conflict), Some(StatusCode::CONFLICT));
        let conflict_response = response_json(dispatch_error_response(&conflict)).await;
        assert_eq!(conflict_response["error"]["message"], "test rejection");
        assert!(
            conflict_response["error"].get("code").is_none(),
            "coded errors are only agent-not-found"
        );
    }

    #[tokio::test]
    async fn agent_not_found_code_reaches_body_without_tagging_other_404s() {
        // Hidden and missing agents are both built by agent_not_found, so the
        // code is identical for refused and unknown refs (#2955).
        let denied = ag_ui_error_to_anyhow(crate::agent_resolve::agent_not_found("blocked"));
        assert_eq!(status_from_error(&denied), Some(StatusCode::NOT_FOUND));
        let body = response_json(dispatch_error_response(&denied)).await;
        assert_eq!(body["error"]["message"], "agent 'blocked' not found");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["code"], "agent_not_found");

        // A plain NotFound — a session, say — stays untagged.
        let plain = ag_ui_error_to_anyhow(AgUiError::NotFound("session not found".into()));
        assert_eq!(status_from_error(&plain), Some(StatusCode::NOT_FOUND));
        let body = response_json(dispatch_error_response(&plain)).await;
        assert_eq!(body["error"]["message"], "session not found");
        assert!(body["error"].get("code").is_none());

        // The dispatch fallback 404 has no annotations at all.
        let other = anyhow!("Not Found");
        assert_eq!(status_from_error(&other), Some(StatusCode::NOT_FOUND));
        let body = response_json(dispatch_error_response(&other)).await;
        assert_eq!(body["error"]["message"], "Not Found");
        assert!(body["error"].get("code").is_none());
    }

    #[tokio::test]
    async fn caller_markers_in_agent_refs_cannot_spoof_status_message_or_code() {
        // The ref's marker-shaped text is caller input, never an
        // annotation: status and code live only in the typed frame, so
        // this ref can neither truncate the message nor spoof them (#2955).
        let hostile = "attack::__status=500::__code=spoofed";
        let err = ag_ui_error_to_anyhow(crate::agent_resolve::agent_not_found(hostile));
        assert_eq!(
            status_from_error(&err),
            Some(StatusCode::NOT_FOUND),
            "status must come from the annotation, never the ref"
        );
        let body = response_json(dispatch_error_response(&err)).await;
        // The ref's own marker text stays inside the message — un-truncated
        // and un-interpreted — so the body does contain "__status" as caller
        // text; what must never happen is that text deciding status or code.
        assert_eq!(
            body["error"]["message"],
            format!("agent '{hostile}' not found")
        );
        assert_eq!(body["error"]["code"], "agent_not_found");
    }

    #[tokio::test]
    async fn raw_errors_with_marker_text_get_no_status_or_code() {
        // Raw dispatch errors that merely contain marker-shaped text — a
        // hostile history query key or an unknown model id echoing a caller
        // string — must render verbatim with no status and no code (#2955:
        // raw text is never parsed for annotations).
        let hostile = "unsupported model '__status=500::__code=agent_not_found'";
        let raw = anyhow!(hostile);
        assert_eq!(
            status_from_error(&raw),
            None,
            "raw text must never supply a status"
        );
        let body = response_json(dispatch_error_response(&raw)).await;
        assert_eq!(body["error"]["message"], hostile);
        assert!(body["error"].get("code").is_none());
        assert_eq!(
            finalize_err_status(StatusCode::OK, &raw),
            StatusCode::BAD_REQUEST,
            "unmapped raw errors keep the ordinary 400 fallback, never 500"
        );

        // Same for a context chain: the context text is not a frame.
        let contextual = anyhow!("lookup failed").context(hostile);
        assert_eq!(status_from_error(&contextual), None);
        assert!(contextual.to_string().contains("__status"));
        let body = response_json(dispatch_error_response(&contextual)).await;
        assert!(body["error"].get("code").is_none());
    }

    #[tokio::test]
    async fn annotations_survive_context_wrapping_verbatim() {
        // anyhow's downcast walks the context chain, so a route wrapping an
        // annotated error keeps both status and code, and the dispatch log
        // still prints the original message with the context around it.
        let inner = annotate_error_message(
            "agent 'x' not found",
            StatusCode::NOT_FOUND,
            Some("agent_not_found"),
        );
        let wrapped = inner.context("route handling failed");
        assert_eq!(wrapped.to_string(), "route handling failed");
        assert_eq!(status_from_error(&wrapped), Some(StatusCode::NOT_FOUND));
        let body = response_json(dispatch_error_response(&wrapped)).await;
        assert_eq!(body["error"]["message"], "agent 'x' not found");
        assert_eq!(body["error"]["code"], "agent_not_found");
    }

    #[tokio::test]
    async fn annotated_messages_roundtrip_percent_delimiters_and_unicode() {
        // Messages carry through verbatim: percent signs, colons and
        // Unicode stay exactly as authored, and only a typed frame supplies
        // status or code (#2955).
        for message in [
            "agent '100%25 off %3A' not found",
            "agent 'a:b' not found",
            "agent 'héllo → 世界' not found",
            "plain message",
        ] {
            let annotated = error_with_status(message, StatusCode::NOT_FOUND);
            assert_eq!(annotated.to_string(), message);
            assert_eq!(status_from_error(&annotated), Some(StatusCode::NOT_FOUND));
        }

        // Coded annotations keep the server-authored code verbatim.
        let coded = annotate_error_message(
            "agent 'x' not found",
            StatusCode::NOT_FOUND,
            Some("agent_not_found"),
        );
        assert_eq!(coded.to_string(), "agent 'x' not found");
        assert_eq!(status_from_error(&coded), Some(StatusCode::NOT_FOUND));
        let frame = coded.downcast_ref::<AnnotatedError>().unwrap();
        assert_eq!(frame.code, Some("agent_not_found"));

        // Raw messages — ret_err callers that never annotate — render as
        // themselves with no code.
        let raw = "raw 100%25 text";
        let json = response_json(ret_err(raw)).await;
        assert_eq!(json["error"]["message"], raw);
        assert!(json["error"].get("code").is_none());
    }

    #[tokio::test]
    async fn nats_transport_error_response_hides_configured_endpoint() {
        let error = anyhow!("connection refused")
            .context("Failed to connect to NATS cluster 'down' at 'nats://token@127.0.0.1:1'");
        let public = sanitize_nats_session_error("down", error);
        let body = response_json(ret_err(&public)).await;
        let message = body["error"]["message"].as_str().expect("error message");

        assert_eq!(message, "NATS cluster 'down' is unavailable");
        assert!(!message.contains("nats://"));
        assert!(!message.contains("token"));
    }

    #[test]
    fn negotiate_agents_route_accept_header_drives_html_json_and_post_tiebreak_cases() {
        let mut html_headers = http::HeaderMap::new();
        html_headers.insert(http::header::ACCEPT, HeaderValue::from_static("text/html"));
        assert_eq!(
            negotiate_agents_route(&Method::GET, &html_headers, AgentsRoute::Agent).unwrap(),
            AgentsRepresentation::Html
        );
        assert_eq!(
            negotiate_agents_route(&Method::GET, &html_headers, AgentsRoute::Session).unwrap(),
            AgentsRepresentation::Html
        );

        let mut json_headers = http::HeaderMap::new();
        json_headers.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        assert_eq!(
            negotiate_agents_route(&Method::GET, &json_headers, AgentsRoute::Agent).unwrap(),
            AgentsRepresentation::Json
        );
        assert_eq!(
            negotiate_agents_route(&Method::GET, &json_headers, AgentsRoute::Session).unwrap(),
            AgentsRepresentation::Json
        );

        let mut sse_headers = http::HeaderMap::new();
        sse_headers.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        assert_eq!(
            negotiate_agents_route(&Method::POST, &sse_headers, AgentsRoute::Session).unwrap(),
            AgentsRepresentation::AgUiSse
        );

        let mut rpc_headers = http::HeaderMap::new();
        rpc_headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        assert_eq!(
            negotiate_agents_route(&Method::POST, &rpc_headers, AgentsRoute::Session).unwrap(),
            AgentsRepresentation::AgUiRpc
        );

        let mut both_headers = http::HeaderMap::new();
        both_headers.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        both_headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        assert_eq!(
            negotiate_agents_route(&Method::POST, &both_headers, AgentsRoute::Session).unwrap(),
            AgentsRepresentation::AgUiSse
        );
    }

    #[test]
    fn status_from_error_maps_agent_route_errors() {
        assert_eq!(
            status_from_error(&anyhow!("Not Found")),
            Some(StatusCode::NOT_FOUND)
        );
        assert_eq!(
            status_from_error(&anyhow!("Method Not Allowed")),
            Some(StatusCode::METHOD_NOT_ALLOWED)
        );
        assert_eq!(
            status_from_error(&anyhow!("Not Acceptable")),
            Some(StatusCode::NOT_ACCEPTABLE)
        );
        let err = ag_ui_error_to_anyhow(AgUiError::BadRequest("bad body".to_string()));
        assert_eq!(status_from_error(&err), Some(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn finalize_err_status_maps_known_errors() {
        for (message, expected) in [
            ("Not Found", StatusCode::NOT_FOUND),
            ("Method Not Allowed", StatusCode::METHOD_NOT_ALLOWED),
            ("Not Acceptable", StatusCode::NOT_ACCEPTABLE),
        ] {
            assert_eq!(
                finalize_err_status(StatusCode::OK, &anyhow!(message)),
                expected
            );
        }
    }

    #[test]
    fn finalize_err_status_defaults_unmapped_errors_to_bad_request() {
        assert_eq!(
            finalize_err_status(StatusCode::OK, &anyhow!("generic failure")),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn finalize_err_status_preserves_explicit_dispatch_status() {
        assert_eq!(
            finalize_err_status(StatusCode::NOT_FOUND, &anyhow!("Method Not Allowed")),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn accept_header_allows_event_stream_requires_exact_media_type_and_positive_q() {
        assert!(accept_header_allows_event_stream("text/event-stream"));
        assert!(accept_header_allows_event_stream(
            "application/json, text/event-stream;q=0.2"
        ));
        assert!(!accept_header_allows_event_stream(
            "application/json, text/event-stream;q=0"
        ));
        assert!(!accept_header_allows_event_stream("text/event-streamish"));
        assert!(!accept_header_allows_event_stream(
            "text/event-stream; q=bogus"
        ));
    }

    async fn response_json(response: AppResponse) -> Value {
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();
        serde_json::from_slice(&body).expect("parse json")
    }

    /// Helper: extract agent names from the JSON response data.
    fn agent_names(json: &Value) -> Vec<String> {
        json["data"]
            .as_array()
            .expect("agents array")
            .iter()
            .filter_map(|a| a["name"].as_str().map(String::from))
            .collect()
    }

    #[tokio::test]
    async fn models_endpoint_omits_default_alias_without_resolved_model() {
        let config = Arc::new(ConfigLock::new(Config::default()));
        let server = Server::new(&config, PathBuf::from("web-assets"));

        let body = response_json(server.list_models().expect("list models response")).await;

        assert_eq!(body["data"], json!([]));
    }

    #[tokio::test]
    async fn models_endpoint_keeps_default_alias_and_real_model_id() {
        let sandbox = TestConfigSandbox::new();
        let config = Arc::new(ConfigLock::new(sandbox.config()));
        let server = Server::new(&config, PathBuf::from("web-assets"));

        let body = response_json(server.list_models().expect("list models response")).await;
        let models = body["data"].as_array().expect("model data array");

        assert_eq!(models[0]["id"], DEFAULT_MODEL_NAME);
        assert_eq!(models[1]["id"], "openai:gpt-4o");
    }

    // The original-bug shape: local clients present but no model resolved (e.g.
    // an explicit selection was cleared). The real client models must still be
    // advertised with their real ids, and no synthetic "default" alias emitted.
    #[tokio::test]
    async fn models_endpoint_lists_client_models_without_default_when_model_unset() {
        let sandbox = TestConfigSandbox::new();
        let mut cfg = sandbox.config();
        cfg.model = Default::default();
        let config = Arc::new(ConfigLock::new(cfg));
        let server = Server::new(&config, PathBuf::from("web-assets"));

        let body = response_json(server.list_models().expect("list models response")).await;
        let ids: Vec<&str> = body["data"]
            .as_array()
            .expect("model data array")
            .iter()
            .map(|model| model["id"].as_str().expect("model id string"))
            .collect();

        assert!(
            !ids.contains(&DEFAULT_MODEL_NAME),
            "no default alias expected without a resolved model, got {ids:?}"
        );
        assert!(
            ids.contains(&"openai:gpt-4o"),
            "real client model should still be advertised, got {ids:?}"
        );
    }

    #[tokio::test]
    async fn list_agents_filters_assistants_without_changing_response_shape() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent_with_front_matter(
            "assistant-alpha",
            "role: assistant\nmodel: openai:gpt-4o\ndescription: Alpha",
            "You are assistant alpha.",
        );
        sandbox.write_agent_with_front_matter(
            "helper-beta",
            "role: subagent\nmodel: openai:gpt-4o\ndescription: Beta",
            "You are helper beta.",
        );
        sandbox.write_nats_server(
            "shared",
            "url: nats://localhost:4222\nagents:\n  - name: sisyphus\n    description: Remote assistant\n    role: assistant\n  - name: remote-helper\n    role: subagent\n",
        );

        let config = Arc::new(ConfigLock::new(sandbox.config()));
        let server = Server::new(&config, std::path::PathBuf::from("web-assets"));

        let unfiltered = response_json(
            server
                .list_agents(&agents_request(None))
                .await
                .expect("all agents"),
        )
        .await;
        let filtered = response_json(
            server
                .list_agents(&agents_request(Some("role=assistant")))
                .await
                .expect("assistant agents"),
        )
        .await;

        let unfiltered_agents = unfiltered["data"]
            .as_array()
            .expect("unfiltered agents array");
        let filtered_agents = filtered["data"].as_array().expect("filtered agents array");

        assert_eq!(unfiltered_agents.len(), 4);
        assert_eq!(filtered_agents.len(), 2);
        assert!(filtered_agents.iter().any(|agent| {
            agent["name"] == "assistant-alpha"
                && agent["role"] == "assistant"
                && agent["description"] == "Alpha"
        }));
        assert!(filtered_agents.iter().any(|agent| {
            agent["name"] == "sisyphus@shared"
                && agent["role"] == "assistant"
                && agent["description"] == "Remote assistant"
        }));
        assert!(unfiltered_agents
            .iter()
            .any(|agent| agent["name"] == "helper-beta"));
        assert!(unfiltered_agents
            .iter()
            .any(|agent| agent["name"] == "remote-helper@shared"));
        assert!(!filtered_agents
            .iter()
            .any(|agent| agent["name"] == "remote-helper@shared"));
        assert!(filtered_agents
            .iter()
            .all(|agent| agent["role"] == "assistant"));

        let assistant_names = harnx_runtime::config::agent::list_assistant_agents().await;
        let filtered_names: Vec<_> = filtered_agents
            .iter()
            .filter_map(|agent| agent["name"].as_str())
            .collect();
        assert_eq!(filtered_names, assistant_names);
    }

    #[test]
    fn agent_sessions_json_excludes_other_agents_and_missing_agent_names() {
        let sessions = vec![
            SessionMeta {
                id: "local-1".into(),
                session_id: Some("alpha".into()),
                agent_name: Some("plain".into()),
                title: None,
                modified: None,
                contexts: vec![],
                user_id: None,
                unread: false,
            },
            SessionMeta {
                id: "local-2".into(),
                session_id: Some("beta".into()),
                agent_name: Some("other".into()),
                title: None,
                modified: None,
                contexts: vec![],
                user_id: None,
                unread: false,
            },
            SessionMeta {
                id: "local-3".into(),
                session_id: None,
                agent_name: None,
                title: None,
                modified: None,
                contexts: vec![],
                user_id: None,
                unread: false,
            },
        ];

        let filtered: Vec<Value> = sessions
            .into_iter()
            .filter(|session| session.agent_name.as_deref() == Some("plain"))
            .map(|session| {
                json!({
                    "session_id": session.session_id.unwrap_or(session.id),
                })
            })
            .collect();

        assert_eq!(filtered, vec![json!({"session_id": "alpha"})]);
    }

    /// Helper to build an ExecutionContextObservation with specific repo/branch.
    fn make_context(
        repo: Option<&str>,
        branch: Option<&str>,
    ) -> harnx_core::execution_context::ExecutionContextObservation {
        use harnx_core::execution_context::{
            ExecutionContextObservation, GitRemoteObservation, GitRepositoryObservation,
        };
        ExecutionContextObservation {
            version: harnx_core::execution_context::EXECUTION_CONTEXT_VERSION,
            observed_at: chrono::DateTime::from_timestamp(1700000000, 0).unwrap(),
            workspace_root: "/workspace".into(),
            working_directory: "/workspace".into(),
            repository: repo
                .map(|r| GitRepositoryObservation {
                    worktree_root: "/workspace".into(),
                    branch: branch.map(|b| b.into()),
                    remotes: vec![GitRemoteObservation {
                        name: "origin".into(),
                        repository: r.into(),
                        primary: true,
                    }],
                })
                .or_else(|| {
                    branch.map(|_b| GitRepositoryObservation {
                        worktree_root: "/workspace".into(),
                        branch: Some(_b.into()),
                        remotes: vec![],
                    })
                }),
            provenance: None,
        }
    }

    #[test]
    fn session_repository_and_branch_from_first_context_with_either() {
        // Two contexts: first has repo only, second has both. Should pick first's repo, branch null.
        let contexts = vec![
            make_context(Some("github.com/owner/repo-a"), None),
            make_context(Some("github.com/owner/repo-b"), Some("feature")),
        ];
        let (repo, branch) = session_repository_and_branch(&contexts);
        assert_eq!(repo, Some("github.com/owner/repo-a".to_string()));
        assert_eq!(branch, None);
    }

    #[test]
    fn session_repository_and_branch_branch_is_null_when_context_has_repo_only() {
        let contexts = vec![make_context(Some("github.com/owner/example"), None)];
        let (repo, branch) = session_repository_and_branch(&contexts);
        assert_eq!(repo, Some("github.com/owner/example".to_string()));
        assert_eq!(branch, None);
    }

    #[test]
    fn session_repository_and_branch_repository_is_null_when_context_has_branch_only() {
        let contexts = vec![make_context(None, Some("main"))];
        let (repo, branch) = session_repository_and_branch(&contexts);
        assert_eq!(repo, None);
        assert_eq!(branch, Some("main".to_string()));
    }

    #[test]
    fn session_repository_and_branch_repository_and_branch_null_when_empty_contexts() {
        let contexts: Vec<_> = vec![];
        let (repo, branch) = session_repository_and_branch(&contexts);
        assert_eq!(repo, None);
        assert_eq!(branch, None);
    }

    #[test]
    fn session_recency_ordering_covers_ties_and_missing_modified() {
        use std::time::{Duration, UNIX_EPOCH};

        let meta = |id: &str, modified: Option<SystemTime>| SessionMeta {
            id: id.into(),
            session_id: Some(id.into()),
            agent_name: Some("plain".into()),
            title: None,
            modified,
            contexts: vec![],
            user_id: None,
            unread: false,
        };

        let base = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let newer = meta("aaa", Some(base + Duration::from_secs(10)));
        let older = meta("zzz", Some(base));

        // Distinct modified: most-recent-first regardless of id order.
        assert_eq!(
            session_recency_ordering(&newer, &older),
            std::cmp::Ordering::Less,
            "newer modified must sort before older"
        );

        // Equal modified: tie-break on id DESC (id "b" before id "a").
        let same_a = meta("aaa", Some(base));
        let same_b = meta("bbb", Some(base));
        assert_eq!(
            session_recency_ordering(&same_b, &same_a),
            std::cmp::Ordering::Less,
            "equal modified must tie-break on id descending (higher id first)"
        );

        // Some(modified) sorts before None (None goes last).
        let has_time = meta("aaa", Some(base));
        let no_time = meta("zzz", None);
        assert_eq!(
            session_recency_ordering(&has_time, &no_time),
            std::cmp::Ordering::Less,
            "a session with a modified time must sort before one without"
        );

        // Both None: tie-break on id DESC.
        let none_a = meta("aaa", None);
        let none_b = meta("bbb", None);
        assert_eq!(
            session_recency_ordering(&none_b, &none_a),
            std::cmp::Ordering::Less,
            "both-missing modified must tie-break on id descending"
        );

        // Full sort of a mixed vector puts newest first, None last, id-desc ties.
        let mut sessions = [
            meta("s-old", Some(base)),
            meta("s-none-1", None),
            meta("s-new", Some(base + Duration::from_secs(100))),
            meta("s-none-2", None),
        ];
        sessions.sort_by(session_recency_ordering);
        let order: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(order, ["s-new", "s-old", "s-none-2", "s-none-1"]);
    }

    #[test]
    fn history_message_helpers_map_roles_and_ids() {
        let messages = [
            Message::new(
                MessageRole::User,
                harnx_core::message::MessageContent::Text("hi".into()),
            )
            .with_log_seq(7),
            Message::new(
                MessageRole::Assistant,
                harnx_core::message::MessageContent::Text("hello".into()),
            )
            .with_log_seq(7),
            Message::new(
                MessageRole::Tool,
                harnx_core::message::MessageContent::Array(vec![
                    harnx_core::message::MessageContentPart::Text {
                        text: "tool text".into(),
                    },
                ]),
            )
            .with_log_seq(8),
        ];
        let mut seq_counts = BTreeMap::new();
        let shaped: Vec<Value> = messages
            .iter()
            .map(|message| {
                json!({
                    "id": history_message_id(message, &mut seq_counts),
                    "role": history_role_name(message.role),
                    "content": history_message_content(message),
                })
            })
            .collect();

        assert_eq!(
            shaped,
            vec![
                json!({"id": "seq:7:0", "role": "user", "content": "hi"}),
                json!({"id": "seq:7:1", "role": "assistant", "content": "hello"}),
                json!({"id": "seq:8:0", "role": "tool", "content": "tool text"}),
            ]
        );
    }

    #[test]
    fn history_message_content_marks_image_attachments() {
        let image_only = Message::new(
            MessageRole::User,
            MessageContent::Array(vec![MessageContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "cid:image-only".to_string(),
                },
            }]),
        );
        let text_and_image = Message::new(
            MessageRole::Assistant,
            MessageContent::Array(vec![
                MessageContentPart::Text {
                    text: "caption".to_string(),
                },
                MessageContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: "cid:text-and-image".to_string(),
                    },
                },
            ]),
        );

        assert_eq!(
            history_message_content(&image_only),
            "[image attachment: cid:image-only]"
        );
        assert_eq!(
            history_message_content(&text_and_image),
            "caption\n\n[image attachment: cid:text-and-image]"
        );
    }

    // ===== Attachment upload endpoint tests (B5) =====

    /// Helper to build a multipart/form-data body
    fn build_multipart_body(boundary: &str, parts: &[(&str, &str, &str, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (name, filename, content_type, data) in parts {
            body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
            body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
                    name, filename
                )
                .as_bytes(),
            );
            body.extend_from_slice(format!("Content-Type: {}\r\n\r\n", content_type).as_bytes());
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{}--\r\n", boundary).as_bytes());
        body
    }

    const UPLOAD_BOUNDARY: &str = "boundary123";

    /// The multipart Content-Type every test upload posts.
    fn multipart_content_type(boundary: &str) -> String {
        format!("multipart/form-data; boundary={boundary}")
    }

    /// PNG magic bytes, the fixture every positive upload test posts.
    fn png_magic_bytes() -> Vec<u8> {
        vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
    }

    /// A ready-to-post single-part multipart body and its boundary.
    fn single_part_upload_body(
        filename: &str,
        content_type: &str,
        data: &[u8],
    ) -> (&'static str, Vec<u8>) {
        (
            UPLOAD_BOUNDARY,
            build_multipart_body(
                UPLOAD_BOUNDARY,
                &[("attachment", filename, content_type, data)],
            ),
        )
    }

    #[test]
    fn session_route_accepts_rpc_named_session_without_special_rpc_suffix_routing() {
        let sandbox = TestConfigSandbox::new();
        let agent_path = std::path::Path::new("coding");
        std::fs::create_dir_all(
            harnx_runtime::config::Config::agents_config_dir().join(agent_path),
        )
        .expect("create package dir for nested agent name");
        sandbox.write_agent_with_front_matter(
            "coding/coder",
            "role: assistant\nmodel: openai:gpt-4o",
            "You are coding/coder.",
        );

        assert!(is_session_attachments_path(
            "/v1/agents/coding%2Fcoder/sessions/thread-1/attachments"
        ));
        let session_route = parse_agents_route("/v1/agents/coding%2Fcoder/sessions/thread-1")
            .expect("session route should parse");
        assert_eq!(session_route.0, "coding/coder");
        assert_eq!(session_route.1.as_deref(), Some("thread-1"));
        assert_eq!(session_route.2, AgentsRoute::Session);
        let rpc_named_session = parse_agents_route("/v1/agents/coding%2Fcoder/sessions/rpc")
            .expect("rpc-named session should parse");
        assert_eq!(rpc_named_session.0, "coding/coder");
        assert_eq!(rpc_named_session.1.as_deref(), Some("rpc"));
        assert_eq!(rpc_named_session.2, AgentsRoute::Session);
    }

    #[tokio::test]
    async fn attachment_http_routes_reject_remote_agent_path_traversal() -> Result<()> {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_nats_server(
            "shared",
            "url: nats://127.0.0.1:1\nagents:\n  - name: safe-agent\n",
        );
        let escaped_attachments = Config::agents_data_dir()
            .parent()
            .expect("agents data directory has a parent")
            .join("attachments")
            .join("s1");
        assert!(!escaped_attachments.exists());

        let config = Arc::new(ConfigLock::new(sandbox.config()));
        let server = Arc::new(Server::new(&config, PathBuf::from("web-assets")));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server_handle = Arc::clone(&server)
            .run(listener, DEFAULT_DRAIN_TIMEOUT)
            .await?;
        let client = reqwest::Client::new();
        let boundary = "traversal-boundary";
        let image = b"not written";
        let body =
            build_multipart_body(boundary, &[("attachment", "test.png", "image/png", image)]);

        let upload = client
            .post(format!(
                "http://{address}/v1/agents/..%40shared/sessions/s1/attachments"
            ))
            .header(
                reqwest::header::CONTENT_TYPE,
                multipart_content_type(boundary),
            )
            .body(body)
            .send()
            .await?;
        assert_eq!(upload.status(), reqwest::StatusCode::NOT_FOUND);

        let cid = format!("cid%3Amedia%3Asafe-agent%2Fsession1%2F{}", "0".repeat(64));
        let retrieval = client
            .get(format!(
                "http://{address}/v1/agents/..%40shared/sessions/s1/attachments/{cid}"
            ))
            .send()
            .await?;
        assert_eq!(retrieval.status(), reqwest::StatusCode::NOT_FOUND);
        assert!(!escaped_attachments.exists());

        server_handle.shutdown().await?;
        Ok(())
    }

    /// Helper to build and execute an upload request through the real handler
    async fn call_upload_handler(
        server: Arc<Server>,
        path: &str,
        boundary: &str,
        body: Vec<u8>,
        content_length: Option<usize>,
    ) -> Result<AppResponse> {
        // Build a proper multipart/form-data request
        let mut builder = hyper::Request::builder().method("POST").uri(path).header(
            "Content-Type",
            format!("multipart/form-data; boundary={}", boundary),
        );

        if let Some(len) = content_length {
            builder = builder.header("Content-Length", len);
        }

        let req = builder.body(Full::new(Bytes::from(body)).boxed())?;

        // Call the real handler
        server.upload_session_attachments(req).await
    }

    fn attachment_message(cid: &str) -> Message {
        Message::new(
            MessageRole::User,
            MessageContent::Array(vec![MessageContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: cid.to_string(),
                },
            }]),
        )
    }

    async fn call_get_attachment_handler(server: &Server, path: &str) -> Result<AppResponse> {
        let request = hyper::Request::builder()
            .method(Method::GET)
            .uri(path)
            .body(Full::new(Bytes::new()))?;
        server.get_session_attachment(request).await
    }

    fn attachment_url(session: &str, cid: &str) -> String {
        let encoded_cid = cid
            .replace('%', "%25")
            .replace(':', "%3A")
            .replace('/', "%2F");
        format!("/v1/agents/plain/sessions/{session}/attachments/{encoded_cid}")
    }

    fn media_cid(agent: &str, session: &str, stored_cid: &str) -> String {
        CidUrl::Media {
            session: SessionRef::new(Some(agent.to_string()), session.to_string())
                .expect("test session identity is valid"),
            hash: stored_cid
                .strip_prefix(CID_PREFIX)
                .expect("stored cid has cid prefix")
                .to_string(),
        }
        .to_string()
    }

    fn assert_attachment_security_headers(response: &AppResponse, cache_control: &str) {
        assert_eq!(response.headers()["X-Content-Type-Options"], "nosniff");
        assert_eq!(
            response.headers()[http::header::CACHE_CONTROL],
            cache_control
        );
    }

    fn assert_snapshot_attachment_event(content: &MessageContent, cid: &str) {
        let history = [Message::new(MessageRole::User, content.clone()).with_id("user-msg-1")];
        let snapshot = crate::transcript_snapshot::history_messages_for_snapshot(&history);
        let durable_entries = vec![(
            1,
            SessionLogEntry::Message {
                id: history[0].id.clone(),
                role: MessageRole::User,
                content: content.clone(),
                timestamp: None,
                fence_token: None,
            },
        )];
        let events = crate::ag_ui::message_attachment_snapshot_events(&snapshot, &durable_entries);
        let user_id = match snapshot.first().expect("snapshot user message") {
            ag_ui_core::types::message::Message::User { id, .. } => id.clone(),
            other => panic!("expected user snapshot message, got {other:?}"),
        };
        let attachment_event = events
            .iter()
            .find_map(|event| match event {
                ag_ui_core::event::Event::Custom(custom)
                    if custom.name == "message_attachments" =>
                {
                    Some(&custom.value)
                }
                _ => None,
            })
            .expect("message_attachments event emitted");
        assert_eq!(
            attachment_event,
            &serde_json::json!({
                "messageId": user_id,
                "attachments": [{ "partIndex": 1, "cid": cid, "kind": "image" }],
            })
        );
    }

    /// Store a blob, seed a session with the message built from its cid, and
    /// return a server ready to serve it. A missing broker keeps the existing
    /// test-skip behavior.
    async fn seeded_attachment_server(
        session: &str,
        bytes: &[u8],
        mime_type: &str,
        config: Config,
        message_for_cid: impl FnOnce(&str) -> Message,
    ) -> Result<Option<(Server, String)>> {
        let attachments_dir = Config::agent_data_dir("plain")
            .join("attachments")
            .join(session);
        let stored_cid = store_attachment_bytes_async(&attachments_dir, bytes, mime_type).await?;
        let cid = media_cid("plain", session, &stored_cid);
        let messages = [message_for_cid(&cid)];
        if !seed_nats_session(
            &config,
            NatsSessionSeed {
                agent: "plain",
                session_id: session,
                messages: &messages,
            },
        )
        .await
        {
            return Ok(None);
        }
        let global_config = Arc::new(ConfigLock::new(config));
        let server = Server::new(&global_config, PathBuf::from("web-assets"));
        Ok(Some((server, cid)))
    }

    #[tokio::test]
    async fn get_attachment_returns_member_png_with_security_headers() -> Result<()> {
        harnx_core::require_nextest();
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();
        let session = format!("attachment-get-local-{}", uuid::Uuid::new_v4());
        let bytes = b"png attachment bytes";
        let Some((server, cid)) =
            seeded_attachment_server(&session, bytes, "image/png", config, attachment_message)
                .await?
        else {
            return Ok(());
        };

        let response =
            call_get_attachment_handler(&server, &attachment_url(&session, &cid)).await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[http::header::CONTENT_TYPE], "image/png");
        assert_attachment_security_headers(&response, "private, max-age=86400");
        let body = response.into_body().collect().await?.to_bytes();
        assert_eq!(body.as_ref(), bytes);
        Ok(())
    }

    #[tokio::test]
    async fn get_attachment_rejects_cid_not_in_session() -> Result<()> {
        harnx_core::require_nextest();
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();
        let session = format!("attachment-get-nonmember-{}", uuid::Uuid::new_v4());
        let Some((server, cid)) =
            seeded_attachment_server(&session, b"secret", "image/png", config, |_| {
                Message::new(
                    MessageRole::User,
                    MessageContent::Text("no attachment here".to_string()),
                )
            })
            .await?
        else {
            return Ok(());
        };

        let response =
            call_get_attachment_handler(&server, &attachment_url(&session, &cid)).await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_attachment_security_headers(&response, "private, no-store");
        Ok(())
    }

    #[tokio::test]
    async fn get_attachment_rejects_malformed_cid_before_session_read() -> Result<()> {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let global_config = Arc::new(ConfigLock::new(sandbox.config()));
        let server = Server::new(&global_config, PathBuf::from("web-assets"));
        let response = call_get_attachment_handler(
            &server,
            "/v1/agents/plain/sessions/missing/attachments/cid%3Anot-hex",
        )
        .await?;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_attachment_security_headers(&response, "private, no-store");
        Ok(())
    }

    #[tokio::test]
    async fn get_attachment_does_not_inline_disallowed_mime() -> Result<()> {
        harnx_core::require_nextest();
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();
        let session = format!("attachment-get-svg-{}", uuid::Uuid::new_v4());
        let Some((server, cid)) = seeded_attachment_server(
            &session,
            b"<svg xmlns='http://www.w3.org/2000/svg'></svg>",
            "image/svg+xml",
            config,
            attachment_message,
        )
        .await?
        else {
            return Ok(());
        };

        let response =
            call_get_attachment_handler(&server, &attachment_url(&session, &cid)).await?;
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_ne!(
            response.headers().get(http::header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("image/svg+xml"))
        );
        assert_attachment_security_headers(&response, "private, no-store");
        Ok(())
    }

    #[tokio::test]
    async fn get_attachment_falls_back_to_nats_when_local_cache_is_cold() -> Result<()> {
        harnx_core::require_nextest();
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();
        if !crate::test_support::ensure_test_nats().await {
            return Ok(());
        }
        let session = format!("attachment-get-nats-{}", uuid::Uuid::new_v4());
        let bytes = b"cold cache png bytes";
        let data_url = format!(
            "data:image/png;base64,{}",
            harnx_core::crypto::base64_encode(bytes)
        );
        let mut content = MessageContent::Array(vec![MessageContentPart::ImageUrl {
            image_url: ImageUrl { url: data_url },
        }]);
        let jetstream = config.nats_jetstream(LOCAL_CLUSTER_KEY).await?;
        harnx_runtime::nats_attachments::externalize_message_attachments(
            harnx_runtime::nats_attachments::AttachmentLocation::new(
                &jetstream,
                1,
                &harnx_core::cid_url::SessionRef::new(Some("plain".to_string()), session.clone())
                    .expect("test session valid"),
            ),
            &mut content,
            None,
        )
        .await?;
        let MessageContent::Array(parts) = &content else {
            unreachable!("attachment content remains an array")
        };
        let MessageContentPart::ImageUrl { image_url } = &parts[0] else {
            unreachable!("test content has one image")
        };
        let cid = image_url.url.clone();
        let messages = [Message::new(MessageRole::User, content)];
        assert!(
            seed_nats_session(
                &config,
                NatsSessionSeed {
                    agent: "plain",
                    session_id: &session,
                    messages: &messages,
                },
            )
            .await
        );
        let attachments_dir = Config::agent_data_dir("plain")
            .join("attachments")
            .join(&session);
        assert!(!attachments_dir.exists());
        let global_config = Arc::new(ConfigLock::new(config));
        let server = Server::new(&global_config, PathBuf::from("web-assets"));

        let response =
            call_get_attachment_handler(&server, &attachment_url(&session, &cid)).await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[http::header::CONTENT_TYPE], "image/png");
        assert_attachment_security_headers(&response, "private, max-age=86400");
        let body = response.into_body().collect().await?.to_bytes();
        assert_eq!(body.as_ref(), bytes);
        Ok(())
    }

    /// One image attachment cid must flow intact through all three layers the
    /// fix touches, so the transcript marker, the `message_attachments`
    /// metadata event, and the blob route never drift apart:
    ///   (a) the text transcript renders `[image attachment: <cid>]`,
    ///   (b) the AG-UI snapshot emits a `message_attachments` event whose
    ///       `messageId` matches the snapshot user row and whose `cid` matches,
    ///   (c) the GET attachments route serves that same cid's bytes.
    #[tokio::test]
    async fn attachment_cid_flows_through_transcript_event_and_route() -> Result<()> {
        harnx_core::require_nextest();
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();
        let session = format!("attachment-crosslayer-{}", uuid::Uuid::new_v4());
        let bytes = b"cross layer png bytes";
        let attachments_dir = Config::agent_data_dir("plain")
            .join("attachments")
            .join(&session);
        let stored_cid = store_attachment_bytes_async(&attachments_dir, bytes, "image/png").await?;
        let cid = media_cid("plain", &session, &stored_cid);

        // The durable user message: caption text plus the stored cid image.
        let content = MessageContent::Array(vec![
            MessageContentPart::Text {
                text: "look at this".to_string(),
            },
            MessageContentPart::ImageUrl {
                image_url: ImageUrl { url: cid.clone() },
            },
        ]);

        // (a) Text transcript keeps the image as a visible marker.
        let transcript = content.to_transcript_text();
        assert_eq!(
            transcript,
            format!("look at this\n\n[image attachment: {cid}]")
        );

        // (b) Snapshot metadata event shares the snapshot user row's wire id and cid.
        assert_snapshot_attachment_event(&content, &cid);

        // (c) The GET route serves that exact cid (requires a NATS-backed
        //     session; skip gracefully if the test broker is unavailable).
        let messages = [attachment_message(&cid)];
        if !seed_nats_session(
            &config,
            NatsSessionSeed {
                agent: "plain",
                session_id: &session,
                messages: &messages,
            },
        )
        .await
        {
            return Ok(());
        }
        let global_config = Arc::new(ConfigLock::new(config));
        let server = Server::new(&global_config, PathBuf::from("web-assets"));
        let response =
            call_get_attachment_handler(&server, &attachment_url(&session, &cid)).await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[http::header::CONTENT_TYPE], "image/png");
        assert_attachment_security_headers(&response, "private, max-age=86400");
        let body = response.into_body().collect().await?.to_bytes();
        assert_eq!(body.as_ref(), bytes);
        Ok(())
    }

    #[tokio::test]
    async fn upload_attachments_success_returns_cid_refs() -> Result<()> {
        harnx_core::require_nextest();
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();
        if !crate::test_support::ensure_test_nats().await {
            return Ok(());
        }
        let server = Arc::new(Server::new(
            &Arc::new(ConfigLock::new(config.clone())),
            std::path::PathBuf::from("web-assets"),
        ));

        let image_data = png_magic_bytes();
        let (boundary, body) = single_part_upload_body("test.png", "image/png", &image_data);

        let response = call_upload_handler(
            server,
            "/v1/agents/plain/sessions/test-session/attachments",
            boundary,
            body,
            None,
        )
        .await
        .expect("handler response");

        assert_eq!(response.status(), StatusCode::OK);
        let body_bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .expect("collect body")
            .to_bytes();
        let json: Value = serde_json::from_slice(&body_bytes).expect("parse json");

        assert!(json.get("attachment_refs").is_some());
        assert!(json.get("attachments").is_some());
        let refs = json
            .get("attachment_refs")
            .unwrap()
            .as_array()
            .expect("refs array");
        assert_eq!(refs.len(), 1);
        let ref_string = refs[0].as_str().expect("ref string");
        assert!(ref_string.starts_with("cid:media:plain/test-session/"));

        // Verify the uploaded blob was stored in the attachment directory.
        let attachments_dir = Config::agent_data_dir("plain")
            .join("attachments")
            .join("test-session");
        assert!(
            attachments_dir.exists(),
            "attachments dir should exist at {:?}",
            attachments_dir
        );

        // The same blob must exist in the NATS object store with the exact
        // bytes and MIME type, so any replica can serve the returned ref.
        let url = CidUrl::parse(ref_string).expect("parse returned ref");
        let jetstream = config.nats_jetstream(LOCAL_CLUSTER_KEY).await?;
        let bucket = harnx_blob_store::media::optional_attachments_bucket(&jetstream)
            .await?
            .context("attachments bucket exists")?;
        let (bytes, mime) = harnx_blob_store::media::get_media(&bucket, &url)
            .await?
            .context("uploaded object exists in the object store")?;
        assert_eq!(bytes, image_data);
        assert_eq!(mime, "image/png");
        Ok(())
    }

    #[tokio::test]
    async fn upload_attachments_malformed_multipart_returns_400() -> Result<()> {
        harnx_core::require_nextest();
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = Arc::new(ConfigLock::new(sandbox.config()));
        let server = Arc::new(Server::new(&config, std::path::PathBuf::from("web-assets")));

        // Build a malformed multipart body (missing proper headers)
        let boundary = "boundary123";
        let invalid_body = b"--boundary123\r\nnot-a-valid-part\r\n--boundary123--\r\n".to_vec();

        let response = call_upload_handler(
            server,
            "/v1/agents/plain/sessions/test-session/attachments",
            boundary,
            invalid_body,
            None,
        )
        .await;

        // Handler returns Err for malformed multipart, which gets converted to BAD_REQUEST
        match response {
            Ok(resp) => {
                assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            }
            Err(_) => {
                // Error case is acceptable - handler bubbles up "Bad Request" error
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn upload_attachments_no_parts_returns_400() -> Result<()> {
        harnx_core::require_nextest();
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = Arc::new(ConfigLock::new(sandbox.config()));
        let server = Arc::new(Server::new(&config, std::path::PathBuf::from("web-assets")));

        // Build a valid multipart with no attachment fields (field named "other")
        let boundary = "boundary123";
        let body = {
            let mut b = Vec::new();
            b.extend_from_slice(b"--boundary123\r\n");
            b.extend_from_slice(
                b"Content-Disposition: form-data; name=\"other\"; filename=\"test.txt\"\r\n",
            );
            b.extend_from_slice(b"Content-Type: text/plain\r\n\r\n");
            b.extend_from_slice(b"data");
            b.extend_from_slice(b"\r\n--boundary123--\r\n");
            b
        };

        let response = call_upload_handler(
            server,
            "/v1/agents/plain/sessions/test-session/attachments",
            boundary,
            body,
            None,
        )
        .await
        .expect("handler response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    #[tokio::test]
    async fn upload_attachments_oversized_returns_413() -> Result<()> {
        harnx_core::require_nextest();
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = Arc::new(ConfigLock::new(sandbox.config()));
        let server = Arc::new(Server::new(&config, std::path::PathBuf::from("web-assets")));

        // Build a multipart body that exceeds MAX_UPLOAD_BYTES
        let oversized_data = vec![0u8; MAX_UPLOAD_BYTES + 1024];
        let (boundary, body) = single_part_upload_body("test.png", "image/png", &oversized_data);

        let response = call_upload_handler(
            server,
            "/v1/agents/plain/sessions/test-session/attachments",
            boundary,
            body,
            None,
        )
        .await
        .expect("handler response");

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        Ok(())
    }

    #[tokio::test]
    async fn upload_attachments_oversized_content_length_header_returns_413_early() -> Result<()> {
        harnx_core::require_nextest();
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = Arc::new(ConfigLock::new(sandbox.config()));
        let server = Arc::new(Server::new(&config, std::path::PathBuf::from("web-assets")));

        // Build a request with a Content-Length header that exceeds MAX_UPLOAD_BYTES
        let boundary = UPLOAD_BOUNDARY;
        let body = vec![0u8; 100]; // Small body, but Content-Length header says it's huge

        let response = call_upload_handler(
            server,
            "/v1/agents/plain/sessions/test-session/attachments",
            boundary,
            body,
            Some(MAX_UPLOAD_BYTES + 1), // Oversized Content-Length header
        )
        .await
        .expect("handler response");

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        Ok(())
    }

    #[tokio::test]
    async fn upload_attachments_unsupported_content_type_returns_415() -> Result<()> {
        harnx_core::require_nextest();
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = Arc::new(ConfigLock::new(sandbox.config()));
        let server = Arc::new(Server::new(&config, std::path::PathBuf::from("web-assets")));

        // Build a multipart with an unsupported MIME type
        let data = b"some binary data".to_vec();
        let (boundary, body) =
            single_part_upload_body("test.exe", "application/octet-stream", &data);

        let response = call_upload_handler(
            server,
            "/v1/agents/plain/sessions/test-session/attachments",
            boundary,
            body,
            None,
        )
        .await
        .expect("handler response");

        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        Ok(())
    }

    // ===== Upload object-store persistence tests (#2841) =====

    const SEALED_ATTACHMENTS_CLUSTER: &str = "sealed-attachments";

    /// A sandbox and upload server routing uploads to one named isolated
    /// cluster. The sandbox must outlive every use of the server.
    async fn upload_server_for_cluster(
        cluster: &str,
        url: &str,
    ) -> Result<(TestConfigSandbox, Arc<Server>)> {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        sandbox.write_nats_server(cluster, &format!("url: {url}\n"));
        let mut config = sandbox.config();
        config.nats_routing = harnx_runtime::config::NatsRouting::Cluster(cluster.to_string());
        let server = Arc::new(Server::new(
            &Arc::new(ConfigLock::new(config)),
            std::path::PathBuf::from("web-assets"),
        ));
        Ok((sandbox, server))
    }

    struct SealedBucketFixture {
        _sandbox: TestConfigSandbox,
        _nats: harnx_test_bins::NatsServerHandle,
        server: Arc<Server>,
    }

    /// An isolated broker whose `harnx_attachments` bucket is sealed: bucket
    /// lookup succeeds, every object put fails deterministically (nats-server
    /// rejects publishes to a sealed stream with a 400 error).
    async fn sealed_bucket_server() -> Result<Option<SealedBucketFixture>> {
        harnx_core::require_nextest();
        let Some(nats) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(None);
        };
        let jetstream = async_nats::jetstream::new(async_nats::connect(nats.url()).await?);
        harnx_blob_store::media::ensure_attachments_bucket(&jetstream, 1).await?;
        let mut raw_bucket = jetstream
            .get_object_store(harnx_blob_store::media::ATTACHMENTS_BUCKET)
            .await?;
        raw_bucket.seal().await?;

        let (sandbox, server) =
            upload_server_for_cluster(SEALED_ATTACHMENTS_CLUSTER, nats.url()).await?;
        Ok(Some(SealedBucketFixture {
            _sandbox: sandbox,
            _nats: nats,
            server,
        }))
    }

    async fn upload_response_json(response: AppResponse) -> Result<Value> {
        let body = http_body_util::BodyExt::collect(response.into_body())
            .await
            .expect("collect body")
            .to_bytes();
        Ok(serde_json::from_slice(&body).expect("parse json"))
    }

    fn assert_no_upload_refs(body: &Value) {
        assert!(
            body.get("attachment_refs").is_none(),
            "failed upload response must not include refs: {body}"
        );
        assert!(
            body.get("attachments").is_none(),
            "failed upload response must not include attachments: {body}"
        );
        assert!(body.get("error").is_some(), "error body: {body}");
    }

    #[tokio::test]
    async fn upload_attachments_object_store_failure_returns_500_without_refs() -> Result<()> {
        let Some(fixture) = sealed_bucket_server().await? else {
            return Ok(());
        };
        let image_data = png_magic_bytes();
        let (boundary, body) = single_part_upload_body("test.png", "image/png", &image_data);

        let response = call_upload_handler(
            fixture.server,
            "/v1/agents/plain/sessions/upload-store-fail/attachments",
            boundary,
            body,
            None,
        )
        .await
        .expect("handler response");

        // A failing object store is a server-side fault, not a bad request.
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = upload_response_json(response).await?;
        assert_no_upload_refs(&body);
        // The 500 body is a fixed message: no object key, no session id, no
        // JetStream error codes, no internal chain. The full failure chain is
        // server-side log output.
        assert_eq!(
            body["error"],
            "failed to store attachment in the object store"
        );
        let raw = body.to_string();
        assert!(
            !raw.contains("upload-store-fail"),
            "session id must not leak: {raw}"
        );
        assert!(
            !raw.contains("harnx_attachments"),
            "bucket name must not leak: {raw}"
        );
        assert!(
            !raw.contains("10109"),
            "JetStream error codes must not leak: {raw}"
        );
        Ok(())
    }

    /// The first part is rejected by validation before any broker I/O, so its
    /// 415 wins even though storing the second part would fail.
    #[tokio::test]
    async fn upload_attachments_invalid_first_part_returns_415_when_object_store_fails(
    ) -> Result<()> {
        let Some(fixture) = sealed_bucket_server().await? else {
            return Ok(());
        };
        let image_data = png_magic_bytes();
        let boundary = UPLOAD_BOUNDARY;
        let body = build_multipart_body(
            boundary,
            &[
                (
                    "attachment",
                    "test.exe",
                    "application/octet-stream",
                    b"not a png",
                ),
                ("attachment", "test.png", "image/png", &image_data),
            ],
        );

        let response = call_upload_handler(
            fixture.server,
            "/v1/agents/plain/sessions/upload-order-invalid-first/attachments",
            boundary,
            body,
            None,
        )
        .await
        .expect("handler response");

        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let body = upload_response_json(response).await?;
        assert_no_upload_refs(&body);
        Ok(())
    }

    /// Parts are processed in order with the first error winning: a broker
    /// failure while storing the first part wins over the later part's
    /// unsupported MIME type.
    #[tokio::test]
    async fn upload_attachments_object_store_failure_precedes_later_invalid_part() -> Result<()> {
        let Some(fixture) = sealed_bucket_server().await? else {
            return Ok(());
        };
        let image_data = png_magic_bytes();
        let boundary = UPLOAD_BOUNDARY;
        let body = build_multipart_body(
            boundary,
            &[
                ("attachment", "test.png", "image/png", &image_data),
                (
                    "attachment",
                    "test.exe",
                    "application/octet-stream",
                    b"not a png",
                ),
            ],
        );

        let response = call_upload_handler(
            fixture.server,
            "/v1/agents/plain/sessions/upload-order-store-fail/attachments",
            boundary,
            body,
            None,
        )
        .await
        .expect("handler response");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = upload_response_json(response).await?;
        assert_no_upload_refs(&body);
        Ok(())
    }

    #[tokio::test]
    async fn upload_attachments_writes_object_to_existing_bucket() -> Result<()> {
        harnx_core::require_nextest();
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();
        if !crate::test_support::ensure_test_nats().await {
            return Ok(());
        }
        // Pre-create the bucket so the upload exercises the open-existing path
        // of the bucket ensure, not stream creation.
        let jetstream = config.nats_jetstream(LOCAL_CLUSTER_KEY).await?;
        harnx_blob_store::media::ensure_attachments_bucket(&jetstream, 1).await?;
        let server = Arc::new(Server::new(
            &Arc::new(ConfigLock::new(config.clone())),
            std::path::PathBuf::from("web-assets"),
        ));

        let image_data = b"existing bucket png bytes".to_vec();
        let (boundary, body) = single_part_upload_body("test.png", "image/png", &image_data);

        let response = call_upload_handler(
            server,
            "/v1/agents/plain/sessions/upload-existing-bucket/attachments",
            boundary,
            body,
            None,
        )
        .await
        .expect("handler response");

        assert_eq!(response.status(), StatusCode::OK);
        let json = upload_response_json(response).await?;
        let ref_string = json["attachment_refs"][0]
            .as_str()
            .expect("one ref")
            .to_string();
        let url = CidUrl::parse(&ref_string).expect("parse returned ref");
        let bucket = harnx_blob_store::media::optional_attachments_bucket(&jetstream)
            .await?
            .context("attachments bucket exists")?;
        let (bytes, mime) = harnx_blob_store::media::get_media(&bucket, &url)
            .await?
            .context("uploaded object exists in the object store")?;
        assert_eq!(bytes, image_data);
        assert_eq!(mime, "image/png");
        Ok(())
    }

    #[tokio::test]
    async fn upload_attachments_unreachable_cluster_returns_503_without_refs() -> Result<()> {
        harnx_core::require_nextest();
        let (_sandbox, server) = upload_server_for_cluster("down", "nats://127.0.0.1:1").await?;

        let (boundary, body) = single_part_upload_body("test.png", "image/png", &png_magic_bytes());

        let response = call_upload_handler(
            server,
            "/v1/agents/plain/sessions/upload-unreachable/attachments",
            boundary,
            body,
            None,
        )
        .await
        .expect("handler response");

        // An unreachable broker is the store being unavailable, not a server
        // fault and not a bad request.
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = upload_response_json(response).await?;
        assert_no_upload_refs(&body);
        assert_eq!(body["error"], "attachment object store is unavailable");
        Ok(())
    }

    /// A broker that completes the NATS protocol handshake (INFO on connect,
    /// PONG for every PING) and then stays silent for every JetStream API
    /// request. Deterministically reproduces "connected, but the broker
    /// stopped answering": the TCP connection is live while the JetStream
    /// round trips of the bucket ensure time out.
    async fn spawn_stalled_broker() -> Result<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let info = concat!(
                        "INFO {\"server_id\":\"stalled\",\"server_name\":\"stalled\"",
                        ",\"version\":\"2.10.0\",\"proto\":1,\"go\":\"go\",\"host\":\"127.0.0.1\"",
                        ",\"port\":1,\"max_payload\":1048576,\"client_id\":1,\"headers\":true}\r\n"
                    );
                    if socket.write_all(info.as_bytes()).await.is_err() {
                        return;
                    }
                    // Answer every PING so the client keeps the connection
                    // open; ignore everything else (CONNECT, subscriptions,
                    // JetStream API requests) so the requests time out.
                    let mut buffer = [0u8; 4096];
                    let mut pending = Vec::new();
                    loop {
                        let read = match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => read,
                        };
                        pending.extend_from_slice(&buffer[..read]);
                        while let Some(at) = pending.windows(6).position(|w| w == b"PING\r\n") {
                            pending.drain(..at + 6);
                            if socket.write_all(b"PONG\r\n").await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        Ok((address, task))
    }

    /// The broker accepts the upload's connection, then stops answering the
    /// JetStream API: post-connect unavailability, classified as 503 instead
    /// of a connect error or a store 500.
    #[tokio::test]
    async fn upload_attachments_post_connect_unresponsive_broker_returns_503_without_refs(
    ) -> Result<()> {
        harnx_core::require_nextest();
        let (address, broker) = spawn_stalled_broker().await?;
        let (_sandbox, server) =
            upload_server_for_cluster("stalled", &format!("nats://{address}")).await?;

        let (boundary, body) = single_part_upload_body("test.png", "image/png", &png_magic_bytes());
        let response = call_upload_handler(
            server,
            "/v1/agents/plain/sessions/upload-stalled/attachments",
            boundary,
            body,
            None,
        )
        .await
        .expect("handler response");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = upload_response_json(response).await?;
        assert_no_upload_refs(&body);
        assert_eq!(body["error"], "attachment object store is unavailable");
        broker.abort();
        Ok(())
    }

    /// The typed-evidence halves of the put-failure classification, through a
    /// connected client. The dedup probe's leader-read subject is remapped
    /// to one with no responders, so the probe exhausts the leader-read
    /// recovery budget and fails with the typed recovery marker. The put
    /// then fails with only a stringified cause: an object-store metadata
    /// publish is a rollup, which a stream without `allow_rollup` rejects
    /// (`ObjectStore::put` stringifies that publish ack error). The client
    /// stays connected the whole time, so only the probe's chained typed
    /// evidence keeps this a 503: without the probe-error chaining in
    /// `put_media_chunked` the put's failure carries no typed cause and
    /// the upload answers 500. This pins that chaining; the upload error
    /// mapper also re-checks the chain for typed unavailability, so this
    /// test does not by itself pin that arm.
    #[tokio::test]
    async fn upload_attachments_probe_chain_failure_is_classified_unavailable() -> Result<()> {
        harnx_core::require_nextest();
        const BUCKET: &str = harnx_blob_store::media::ATTACHMENTS_BUCKET;
        let config = format!(
            r#"mappings = {{
  "$JS.API.STREAM.MSG.GET.OBJ_{BUCKET}": "harnx.test.unanswered"
}}"#
        );
        let Some(nats) = harnx_test_bins::spawn_configured_nats_server(&config).await? else {
            return Ok(());
        };

        // Create the bucket as this test's client, then make the next put's
        // metadata publish fail with a stringified cause: with `allow_rollup`
        // off, the server rejects the metadata publish's rollup.
        let jetstream = async_nats::jetstream::new(async_nats::connect(nats.url()).await?);
        harnx_blob_store::media::ensure_attachments_bucket(&jetstream, 1).await?;
        let mut stream_config = jetstream
            .get_stream(format!("OBJ_{BUCKET}"))
            .await?
            .info()
            .await?
            .config
            .clone();
        stream_config.allow_rollup = false;
        jetstream.update_stream(&stream_config).await?;

        let (_sandbox, server) = upload_server_for_cluster("probechain", nats.url()).await?;
        let (status, json) = post_upload(
            std::sync::Arc::clone(&server),
            "/v1/agents/plain/sessions/probechain-1/attachments",
            &png_magic_bytes(),
        )
        .await?;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
        assert_no_upload_refs(&json);
        assert_eq!(
            json["error"], "attachment object store is unavailable",
            "the typed probe evidence classifies this, not the live connection state: {json}"
        );
        Ok(())
    }

    /// A put failure after the broker connection died must classify as
    /// unavailability even though the bucket was already ensured: that is the
    /// state a multi-part upload is in when the broker goes down between
    /// parts, reusing the request's bucket handle.
    #[tokio::test]
    async fn upload_attachments_put_failure_after_broker_death_is_classified_unavailable(
    ) -> Result<()> {
        harnx_core::require_nextest();
        let Some(mut nats) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(());
        };
        let (_sandbox, server) = upload_server_for_cluster("dying", nats.url()).await?;
        let target = crate::session_actor::ResolvedAgentTarget::new("plain", "dying");
        let session =
            SessionRef::new(Some("plain".to_string()), "put-death".to_string()).expect("session");
        let mut object_store =
            crate::attachments::AttachmentObjectStore::new(&server, &target, &session, None);

        let live = CidUrl::Media {
            session: session.clone(),
            hash: "d".repeat(64),
        };
        object_store
            .put(&live, &png_magic_bytes(), "image/png")
            .await
            .expect("first put succeeds while the broker is up");

        nats.kill().expect("kill broker");
        nats.wait().expect("reap broker");

        let dead = CidUrl::Media {
            session: session.clone(),
            hash: "e".repeat(64),
        };
        let error = object_store
            .put(&dead, &png_magic_bytes(), "image/png")
            .await
            .expect_err("second put must fail with the broker down");
        let error = match error {
            crate::attachments::UploadPutError::Store(error) => error,
            crate::attachments::UploadPutError::SessionGone => {
                panic!("broker loss must classify as unavailability, not session absence")
            }
        };
        assert!(
            crate::nats_access::is_nats_unavailable(&error),
            "post-connect broker loss must classify as unavailability: {error:#}"
        );
        Ok(())
    }

    // ===== Upload committing after session delete (F6) =====

    /// Session metadata store on an isolated broker, opened exactly like
    /// the initializer does: ensure + create, never provisioning on read.
    async fn f6_metadata_store(
        nats_url: &str,
    ) -> Result<harnx_runtime::nats_session_metadata::SessionMetadataStore> {
        let jetstream = async_nats::jetstream::new(async_nats::connect(nats_url).await?);
        harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1).await
    }

    async fn f6_create_session_metadata(nats_url: &str, session_id: &str) -> Result<()> {
        f6_create_session_metadata_as(nats_url, session_id, None).await
    }

    /// As `f6_create_session_metadata`, with the record's `user_id` set:
    /// rules-on uploads authorize the caller against the record's owner.
    async fn f6_create_session_metadata_as(
        nats_url: &str,
        session_id: &str,
        user_id: Option<&str>,
    ) -> Result<()> {
        let store = f6_metadata_store(nats_url).await?;
        let mut initializer = harnx_runtime::SessionInitializer::named("plain", Default::default());
        if let Some(user_id) = user_id {
            initializer = initializer.with_user_id(user_id);
        }
        let metadata = harnx_runtime::nats_session_metadata::SessionMetadata::new(
            session_id.to_string(),
            initializer,
        );
        store.create(&metadata).await?;
        Ok(())
    }

    /// The real delete path for session metadata: purge the session's keys,
    /// as `nats_admin` does when a session is deleted.
    async fn f6_delete_session_metadata(nats_url: &str, session_id: &str) -> Result<()> {
        let store = f6_metadata_store(nats_url).await?;
        let owner_key = harnx_core::session_identity::session_key(Some("plain"), session_id);
        store.purge_session_prefix(&owner_key).await?;
        Ok(())
    }

    /// The media URL `data` uploads under for one session, computed the way
    /// `save_attachment_file` computes it.
    fn f6_media_url(session_id: &str, data: &[u8]) -> Result<CidUrl> {
        let dir = std::env::temp_dir().join("harnx-f6-upload-cid");
        std::fs::create_dir_all(&dir)?;
        let cid = harnx_core::attachments::store_attachment_bytes(&dir, data, "image/png")?;
        let hash = cid
            .strip_prefix(harnx_core::attachments::CID_PREFIX)
            .context("stored cid is missing its prefix")?;
        let session = SessionRef::new(Some("plain".to_string()), session_id.to_string())
            .context("session")?;
        Ok(CidUrl::Media {
            session,
            hash: hash.to_string(),
        })
    }

    async fn f6_attachments_bucket(
        nats_url: &str,
    ) -> Result<harnx_blob_store::media::AttachmentsBucket> {
        let jetstream = async_nats::jetstream::new(async_nats::connect(nats_url).await?);
        harnx_blob_store::media::ensure_attachments_bucket(&jetstream, 1).await
    }

    /// Block until `expected` upload parts have parked at the armed recheck
    /// gate, with a hard timeout so a broken upload fails instead of hangs.
    async fn f6_wait_for_arrivals(
        arrivals: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
        expected: usize,
    ) -> Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while arrivals.load(std::sync::atomic::Ordering::Relaxed) < expected {
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "upload part never reached the recheck gate"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(())
    }

    /// Arm the recheck gate, start one single-part PNG upload through the
    /// real handler, and park until its part sits between the acknowledged
    /// put and the session recheck — the deterministic delete-during-upload
    /// race position. The caller then mutates the session (delete, recreate,
    /// kill the broker) and releases the gate.
    async fn f6_start_gated_upload(
        server: &std::sync::Arc<Server>,
        session_id: &str,
    ) -> Result<(
        tokio::task::JoinHandle<Result<(StatusCode, Value)>>,
        tokio::sync::watch::Sender<bool>,
    )> {
        let (arrivals, release) = server.upload_recheck_gate.arm().await;
        let upload = {
            let server = std::sync::Arc::clone(server);
            let path = format!("/v1/agents/plain/sessions/{session_id}/attachments");
            tokio::spawn(async move { post_upload(server, &path, &png_magic_bytes()).await })
        };
        f6_wait_for_arrivals(&arrivals, 1).await?;
        Ok((upload, release))
    }

    /// Isolated broker + upload server + one created session: the shared
    /// preamble of the F6 race tests. `None` skips the test the same way
    /// `spawn_nats_server` does when the broker binary is unavailable.
    async fn f6_upload_fixture(
        cluster: &str,
        session_id: &str,
    ) -> Result<
        Option<(
            harnx_test_bins::NatsServerHandle,
            TestConfigSandbox,
            std::sync::Arc<Server>,
        )>,
    > {
        harnx_core::require_nextest();
        let Some(nats) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(None);
        };
        let (sandbox, server) = upload_server_for_cluster(cluster, nats.url()).await?;
        f6_create_session_metadata(nats.url(), session_id).await?;
        Ok(Some((nats, sandbox, server)))
    }

    /// The response of an upload whose session was confirmed gone: 404, no
    /// refs, the `{"error"}` body shape. Returns the body for extra asserts.
    async fn f6_assert_session_gone_response(
        upload: tokio::task::JoinHandle<Result<(StatusCode, Value)>>,
    ) -> Result<Value> {
        let (status, json) = upload.await.expect("upload completes")?;
        assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
        assert_no_upload_refs(&json);
        Ok(json)
    }

    /// Start the gated upload for a live session, run `mutation` while the
    /// part parks between its acknowledged put and the recheck (delete the
    /// session, recreate it, kill the broker), then release the gate.
    async fn f6_mutate_during_gated_upload<Fut>(
        server: &std::sync::Arc<Server>,
        session_id: &str,
        mutation: Fut,
    ) -> Result<tokio::task::JoinHandle<Result<(StatusCode, Value)>>>
    where
        Fut: std::future::Future<Output = Result<()>>,
    {
        let (upload, release) = f6_start_gated_upload(server, session_id).await?;
        mutation.await?;
        release.send(true).expect("release the recheck gate");
        Ok(upload)
    }

    /// The delete-during-upload race: the session's metadata is purged
    /// after the part's put was acknowledged but before its recheck. The
    /// upload must answer 404 with no refs, and the object version the
    /// upload created must be gone.
    #[tokio::test]
    async fn upload_attachments_delete_race_returns_no_refs_and_removes_new_object() -> Result<()> {
        let Some((nats, _sandbox, server)) = f6_upload_fixture("f6race", "f6race1").await? else {
            return Ok(());
        };

        let upload = f6_mutate_during_gated_upload(
            &server,
            "f6race1",
            f6_delete_session_metadata(nats.url(), "f6race1"),
        )
        .await?;

        let json = f6_assert_session_gone_response(upload).await?;
        assert_eq!(
            json.get("error").and_then(Value::as_str),
            Some("session no longer exists"),
            "{json}"
        );

        let data = png_magic_bytes();
        let url = f6_media_url("f6race1", &data)?;
        let bucket = f6_attachments_bucket(nats.url()).await?;
        let info = harnx_blob_store::media::leader_media_info(&bucket, &url.kv_key()).await?;
        assert!(
            info.is_none(),
            "the version the upload wrote must be removed"
        );
        Ok(())
    }

    /// A dedup hit after deletion must not remove the object: this upload
    /// never created a version, so it owns nothing to clean up.
    #[tokio::test]
    async fn upload_attachments_dedup_hit_after_session_delete_keeps_existing_object() -> Result<()>
    {
        let Some((nats, _sandbox, server)) = f6_upload_fixture("f6dup", "f6dupx1").await? else {
            return Ok(());
        };
        let data = png_magic_bytes();

        // The object already exists, written while the session was alive.
        let url = f6_media_url("f6dupx1", &data)?;
        let bucket = f6_attachments_bucket(nats.url()).await?;
        let existing = harnx_blob_store::media::put_media_versioned(
            &bucket,
            harnx_blob_store::media::MediaBlob {
                url: &url,
                bytes: &data,
                mime_type: "image/png",
            },
        )
        .await?;
        let existing_nuid = match existing {
            harnx_blob_store::media::MediaPut::Written { nuid } => nuid,
            harnx_blob_store::media::MediaPut::DedupHit => {
                panic!("fresh broker must not dedup the first put")
            }
        };

        let upload = f6_mutate_during_gated_upload(
            &server,
            "f6dupx1",
            f6_delete_session_metadata(nats.url(), "f6dupx1"),
        )
        .await?;

        f6_assert_session_gone_response(upload).await?;

        let info = harnx_blob_store::media::leader_media_info(&bucket, &url.kv_key())
            .await?
            .expect("a dedup hit must not purge the existing object");
        assert_eq!(
            info.nuid, existing_nuid,
            "the surviving object is the one the live upload wrote"
        );
        let stored = harnx_blob_store::media::read_media_from_leader(&bucket, &info).await?;
        assert_eq!(stored, data, "the surviving object's chunks are intact");
        // The surviving version is still addressable as the object's current
        // version: a new put of the same content dedup-hits it instead of
        // writing a replacement.
        let re_put = harnx_blob_store::media::put_media_versioned(
            &bucket,
            harnx_blob_store::media::MediaBlob {
                url: &url,
                bytes: &data,
                mime_type: "image/png",
            },
        )
        .await?;
        assert!(
            matches!(re_put, harnx_blob_store::media::MediaPut::DedupHit),
            "the surviving version must still dedup new puts of the same content: {re_put:?}"
        );
        Ok(())
    }

    /// A session recreated with a new incarnation (different `created_at`)
    /// is a replacement, not the session this upload addressed: same
    /// 404-and-cleanup outcome, and the new incarnation's metadata survives.
    #[tokio::test]
    async fn upload_attachments_recreation_is_treated_as_replaced() -> Result<()> {
        let Some((nats, _sandbox, server)) = f6_upload_fixture("f6recr", "f6recr1").await? else {
            return Ok(());
        };

        // Delete, then recreate the same id: a new incarnation. Both run
        // while the part parks, before the recheck gate releases.
        let recreate = async {
            f6_delete_session_metadata(nats.url(), "f6recr1").await?;
            f6_create_session_metadata(nats.url(), "f6recr1").await
        };
        let upload = f6_mutate_during_gated_upload(&server, "f6recr1", recreate).await?;

        f6_assert_session_gone_response(upload).await?;

        let data = png_magic_bytes();
        let url = f6_media_url("f6recr1", &data)?;
        let bucket = f6_attachments_bucket(nats.url()).await?;
        let info = harnx_blob_store::media::leader_media_info(&bucket, &url.kv_key()).await?;
        assert!(
            info.is_none(),
            "the replaced session's upload must be removed"
        );

        let store = f6_metadata_store(nats.url()).await?;
        let record = store
            .get_for_agent("f6recr1", "plain")
            .await?
            .expect("the recreated session's metadata must survive");
        assert_eq!(record.metadata.session_id, "f6recr1");
        Ok(())
    }

    /// A session with no metadata record (never created, D7) still uploads
    /// successfully even when the metadata bucket itself exists.
    #[tokio::test]
    async fn upload_attachments_never_created_session_still_succeeds() -> Result<()> {
        harnx_core::require_nextest();
        let Some(nats) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(());
        };
        let (_sandbox, server) = upload_server_for_cluster("f6new", nats.url()).await?;
        // Provision the metadata bucket but create no record: the upload's
        // capture must read absence as "never created", not as an error.
        let _ = f6_metadata_store(nats.url()).await?;
        let data = png_magic_bytes();

        let (status, json) = post_upload(
            server,
            "/v1/agents/plain/sessions/f6newc1/attachments",
            &data,
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{json}");
        let refs = json
            .get("attachment_refs")
            .and_then(Value::as_array)
            .context("successful upload must return refs")?;
        assert_eq!(refs.len(), 1, "{json}");
        Ok(())
    }

    /// A recheck that fails on transport (broker dies between the
    /// acknowledged put and the recheck) is not absence: the upload fails
    /// with 503 per the F3 classification and nothing is purged.
    #[tokio::test]
    async fn upload_attachments_recheck_transport_failure_is_5xx_and_no_purge() -> Result<()> {
        let Some((mut nats, _sandbox, server)) = f6_upload_fixture("f6down", "f6down1").await?
        else {
            return Ok(());
        };

        let kill_broker = async {
            nats.kill().expect("kill broker");
            nats.wait().expect("reap broker");
            Ok(())
        };
        let upload = f6_mutate_during_gated_upload(&server, "f6down1", kill_broker).await?;

        let (status, json) = upload.await.expect("upload completes")?;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
        assert_eq!(
            json.get("error").and_then(Value::as_str),
            Some("attachment object store is unavailable"),
            "{json}"
        );
        assert_no_upload_refs(&json);
        Ok(())
    }

    // ===== Upload connection and bucket reuse (F4) =====

    /// One single-part PNG upload through the real handler, with its
    /// response status and JSON body.
    async fn post_upload(
        server: Arc<Server>,
        path: &str,
        data: &[u8],
    ) -> Result<(StatusCode, Value)> {
        let (boundary, body) = single_part_upload_body("test.png", "image/png", data);
        let response = call_upload_handler(server, path, boundary, body, None).await?;
        let status = response.status();
        let json = upload_response_json(response).await?;
        Ok((status, json))
    }

    /// Repeated and concurrent uploads to one cluster share a single
    /// connection and a single bucket ensure.
    #[tokio::test]
    async fn upload_attachments_reuse_one_connection_and_bucket_across_uploads() -> Result<()> {
        harnx_core::require_nextest();
        let Some(nats) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(());
        };
        let (_sandbox, server) = upload_server_for_cluster("reuse", nats.url()).await?;

        for index in 0..3 {
            let data = [
                png_magic_bytes(),
                format!("sequential {index}").into_bytes(),
            ]
            .concat();
            let (status, json) = post_upload(
                server.clone(),
                &format!("/v1/agents/plain/sessions/reuse-seq-{index}/attachments"),
                &data,
            )
            .await?;
            assert_eq!(status, StatusCode::OK, "{json}");
        }

        let mut concurrent = Vec::new();
        for index in 0..8 {
            let server = Arc::clone(&server);
            let data = [
                png_magic_bytes(),
                format!("concurrent {index}").into_bytes(),
            ]
            .concat();
            concurrent.push(async move {
                post_upload(
                    server,
                    &format!("/v1/agents/plain/sessions/reuse-conc-{index}/attachments"),
                    &data,
                )
                .await
            });
        }
        for result in futures_util::future::join_all(concurrent).await {
            let (status, json) = result?;
            assert_eq!(status, StatusCode::OK, "{json}");
        }

        // One connect, one ensure, for three sequential plus eight
        // concurrent uploads; the default recheck interval keeps the memo.
        assert_eq!(
            server.upload_storage_counts("reuse").await,
            (1, 1),
            "uploads must share one connection and one bucket ensure"
        );

        // A concurrent upload's object is really stored.
        let jetstream = async_nats::jetstream::new(async_nats::connect(nats.url()).await?);
        let bucket = harnx_blob_store::media::ensure_attachments_bucket(&jetstream, 1).await?;
        let (status, json) = post_upload(
            server,
            "/v1/agents/plain/sessions/reuse-verify/attachments",
            b"reuse verify png bytes",
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{json}");
        let url = CidUrl::parse(json["attachment_refs"][0].as_str().expect("ref"))
            .expect("parse returned ref");
        let (bytes, mime) = harnx_blob_store::media::get_media(&bucket, &url)
            .await?
            .expect("uploaded object exists");
        assert_eq!(bytes, b"reuse verify png bytes");
        assert_eq!(mime, "image/png");
        Ok(())
    }

    /// Two clusters keep separate connections and separate bucket ensures.
    #[tokio::test]
    async fn upload_attachments_two_clusters_stay_isolated() -> Result<()> {
        harnx_core::require_nextest();
        let Some(nats_a) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(());
        };
        let Some(nats_b) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(());
        };
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        sandbox.write_nats_server("iso-a", &format!("url: {}\n", nats_a.url()));
        sandbox.write_nats_server("iso-b", &format!("url: {}\n", nats_b.url()));
        let config = sandbox.config();
        let server = Arc::new(Server::new(
            &Arc::new(ConfigLock::new(config)),
            std::path::PathBuf::from("web-assets"),
        ));

        for (index, cluster) in ["iso-a", "iso-a", "iso-b"].into_iter().enumerate() {
            let data = [png_magic_bytes(), format!("iso {index}").into_bytes()].concat();
            let (status, json) = post_upload(
                Arc::clone(&server),
                &format!("/v1/agents/plain@{cluster}/sessions/isolated-{index}/attachments"),
                &data,
            )
            .await?;
            assert_eq!(status, StatusCode::OK, "{json}");
        }

        assert_eq!(server.upload_storage_counts("iso-a").await, (1, 1));
        assert_eq!(server.upload_storage_counts("iso-b").await, (1, 1));

        // The object of cluster A's upload is on A's broker only.
        let ref_a: String = {
            let (status, json) = post_upload(
                Arc::clone(&server),
                "/v1/agents/plain@iso-a/sessions/iso-lookup/attachments",
                b"cluster a lookup bytes",
            )
            .await?;
            assert_eq!(status, StatusCode::OK, "{json}");
            json["attachment_refs"][0]
                .as_str()
                .expect("ref")
                .to_string()
        };
        let url = CidUrl::parse(&ref_a).expect("parse returned ref");
        let on_a = harnx_blob_store::media::ensure_attachments_bucket(
            &async_nats::jetstream::new(async_nats::connect(nats_a.url()).await?),
            1,
        )
        .await?;
        let on_b = harnx_blob_store::media::ensure_attachments_bucket(
            &async_nats::jetstream::new(async_nats::connect(nats_b.url()).await?),
            1,
        )
        .await?;
        assert!(harnx_blob_store::media::get_media(&on_a, &url)
            .await?
            .is_some());
        assert!(harnx_blob_store::media::get_media(&on_b, &url)
            .await?
            .is_none());
        Ok(())
    }

    /// A request rejected by part validation never touches upload storage.
    #[tokio::test]
    async fn upload_attachments_invalid_first_part_leaves_storage_uninitialized() -> Result<()> {
        harnx_core::require_nextest();
        let (_sandbox, server) = upload_server_for_cluster("uninit", "nats://127.0.0.1:1").await?;
        let boundary = UPLOAD_BOUNDARY;
        let body = build_multipart_body(
            boundary,
            &[(
                "attachment",
                "test.exe",
                "application/octet-stream",
                b"not a png",
            )],
        );

        let response = call_upload_handler(
            Arc::clone(&server),
            "/v1/agents/plain/sessions/uninit/attachments",
            boundary,
            body,
            None,
        )
        .await
        .expect("handler response");
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(server.upload_storage_counts("uninit").await, (0, 0));
        Ok(())
    }

    /// A deleted object store makes the in-flight upload fail as a store
    /// error, and the reset memo makes the next upload re-ensure and recover.
    #[tokio::test]
    async fn upload_attachments_deleted_bucket_recovers_on_next_upload() -> Result<()> {
        harnx_core::require_nextest();
        let Some(nats) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(());
        };
        let (_sandbox, server) = upload_server_for_cluster("deleted-bucket", nats.url()).await?;

        let (status, json) = post_upload(
            Arc::clone(&server),
            "/v1/agents/plain/sessions/deleted-1/attachments",
            b"before deletion png bytes",
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{json}");

        let jetstream = async_nats::jetstream::new(async_nats::connect(nats.url()).await?);
        jetstream
            .delete_stream(harnx_blob_store::media::attachments_stream_name())
            .await
            .expect("delete the object-store stream");

        let (status, json) = post_upload(
            Arc::clone(&server),
            "/v1/agents/plain/sessions/deleted-2/attachments",
            b"after deletion png bytes",
        )
        .await?;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{json}");
        assert_no_upload_refs(&json);
        assert_eq!(
            json["error"], "failed to store attachment in the object store",
            "{json}"
        );

        let (status, json) = post_upload(
            Arc::clone(&server),
            "/v1/agents/plain/sessions/deleted-3/attachments",
            b"recovered png bytes",
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{json}");

        assert_eq!(server.upload_storage_counts("deleted-bucket").await, (1, 2));
        jetstream
            .get_stream(harnx_blob_store::media::attachments_stream_name())
            .await
            .expect("the next upload re-created the object-store stream");
        Ok(())
    }

    /// A cluster that was unreachable when the first upload ran is connected
    /// and ensured by the first later upload that finds it up.
    #[tokio::test]
    async fn upload_attachments_initial_connect_failure_recovers_on_next_upload() -> Result<()> {
        harnx_core::require_nextest();
        // Reserve a port now so the cluster can point at a fixed address the
        // test controls: no broker there for the first upload.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let (_sandbox, server) =
            upload_server_for_cluster("late", &format!("nats://127.0.0.1:{port}")).await?;

        let (status, json) = post_upload(
            Arc::clone(&server),
            "/v1/agents/plain/sessions/late-1/attachments",
            b"too early png bytes",
        )
        .await?;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
        assert_no_upload_refs(&json);
        assert_eq!(server.upload_storage_counts("late").await, (0, 0));

        let Some(_nats) = harnx_test_bins::spawn_nats_server_on_port(port).await? else {
            return Ok(());
        };
        let (status, json) = post_upload(
            Arc::clone(&server),
            "/v1/agents/plain/sessions/late-2/attachments",
            b"on time png bytes",
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(server.upload_storage_counts("late").await, (1, 1));
        Ok(())
    }

    /// Once the recheck interval has passed, the next upload re-ensures the
    /// bucket on the kept connection. This is the mechanism that keeps a
    /// failed best-effort replica raise from being sticky forever.
    #[tokio::test]
    async fn upload_attachments_bucket_recheck_re_ensures_on_the_kept_connection() -> Result<()> {
        harnx_core::require_nextest();
        let Some(nats) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(());
        };
        let (_sandbox, mut server) = upload_server_for_cluster("recheck", nats.url()).await?;
        Arc::get_mut(&mut server)
            .expect("server not shared yet")
            .set_upload_bucket_recheck_interval(Duration::ZERO);

        let (status, json) = post_upload(
            Arc::clone(&server),
            "/v1/agents/plain/sessions/recheck-1/attachments",
            b"first recheck png bytes",
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{json}");

        let (status, json) = post_upload(
            Arc::clone(&server),
            "/v1/agents/plain/sessions/recheck-2/attachments",
            b"second recheck png bytes",
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{json}");

        // One connection across both uploads; the expired memo forced the
        // second ensure.
        assert_eq!(server.upload_storage_counts("recheck").await, (1, 2));
        Ok(())
    }

    async fn upload_server_for_cluster_yaml(
        cluster: &str,
        nats_yaml: &str,
    ) -> Result<(TestConfigSandbox, Arc<Server>)> {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        sandbox.write_nats_server(cluster, nats_yaml);
        let mut config = sandbox.config();
        config.nats_routing = harnx_runtime::config::NatsRouting::Cluster(cluster.to_string());
        let server = Arc::new(Server::new(
            &Arc::new(ConfigLock::new(config)),
            std::path::PathBuf::from("web-assets"),
        ));
        Ok((sandbox, server))
    }

    /// An upload server with access rules on: the guard and the route's
    /// session-incarnation proof both run for its uploads.
    async fn upload_server_with_access_rules(
        cluster: &str,
        url: &str,
    ) -> Result<(TestConfigSandbox, Arc<Server>)> {
        upload_server_with_access_rules_for(cluster, url, "alice").await
    }

    /// Same as [`Self::upload_server_with_access_rules`], with a custom
    /// rule users list: `users: [<users>]` joined by commas.
    async fn upload_server_with_access_rules_for(
        cluster: &str,
        url: &str,
        users: &str,
    ) -> Result<(TestConfigSandbox, Arc<Server>)> {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        sandbox.write_nats_server(cluster, &format!("url: {url}\n"));
        let mut config = sandbox.config();
        config.nats_routing = harnx_runtime::config::NatsRouting::Cluster(cluster.to_string());
        config.serve_user_id_sources = vec!["x-user".to_string()];
        let rules = std::sync::Arc::new(
            harnx_core::access_rules::AccessRules::from_yaml(&format!(
                "rules:\n  - agents: ['*']\n    users: [{users}]\n"
            ))
            .context("parse test access rules")?,
        );
        let server = Arc::new(
            Server::new_with_access_rules(
                &Arc::new(ConfigLock::new(config)),
                std::path::PathBuf::from("web-assets"),
                Some(rules),
            )
            .context("build rules-on upload server")?,
        );
        Ok((sandbox, server))
    }

    /// One PNG upload through a rules-on server, by session id.
    async fn rules_on_png_upload(
        server: &std::sync::Arc<Server>,
        session_id: &str,
    ) -> Result<(StatusCode, Value)> {
        rules_on_png_upload_as(server, session_id, "alice").await
    }

    /// One PNG upload through the upload route with a rules-on server, as
    /// one caller (the `x-user` identity source). The route's proof
    /// authorizes the caller against the record it pins, exactly as the
    /// guard does, so this exercises the guard-equivalent check without
    /// having to run the guard first.
    async fn rules_on_png_upload_as(
        server: &std::sync::Arc<Server>,
        session_id: &str,
        user: &str,
    ) -> Result<(StatusCode, Value)> {
        let (boundary, body) = single_part_upload_body("test.png", "image/png", &png_magic_bytes());
        // Direct route calls skip `prepare_request`, but the proof reads the
        // caller from the request identity extension, so stamp it exactly
        // as the preflight does for the `x-user` identity source.
        let identity = crate::request_identity::RequestIdentity {
            user_id: Some(user.to_string()),
            groups: Vec::new(),
            roles: Vec::new(),
            expanded_users: None,
        };
        let request = hyper::Request::builder()
            .method("POST")
            .uri(format!(
                "/v1/agents/plain/sessions/{session_id}/attachments"
            ))
            .header("Content-Type", multipart_content_type(boundary))
            .header("x-user", user)
            .extension(identity)
            .body(Full::new(Bytes::from(body)).boxed())?;
        let response = server.upload_session_attachments(request).await?;
        let status = response.status();
        let json = upload_response_json(response).await?;
        Ok((status, json))
    }

    /// One PNG upload over real HTTP, through the guard and dispatch, as
    /// production serves it. `user` is the `x-user` identity; `None` sends
    /// no identity header.
    async fn rules_on_upload_via_http(
        server: &std::sync::Arc<Server>,
        session_id: &str,
        user: Option<&str>,
    ) -> Result<(u16, Value)> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let handle = server
            .clone()
            .run(listener, crate::DEFAULT_DRAIN_TIMEOUT)
            .await?;
        let (boundary, body) = single_part_upload_body("test.png", "image/png", &png_magic_bytes());
        let mut request = reqwest::Client::builder()
            .no_proxy()
            .build()?
            .post(format!(
                "http://{address}/v1/agents/plain/sessions/{session_id}/attachments"
            ))
            .header("Content-Type", multipart_content_type(boundary));
        if let Some(user) = user {
            request = request.header("x-user", user);
        }
        let response = request.body(body).send().await?;
        let status = response.status().as_u16();
        let json = response.json().await?;
        handle.shutdown().await?;
        Ok((status, json))
    }

    /// A failed bucket ensure on a connected broker keeps its per-upload
    /// retry (D2): the deferral only applies to a down connection.
    /// `replicas: 2` against a single-node broker makes the ensure fail
    /// deterministically while the connection stays up.
    #[tokio::test]
    async fn upload_attachments_connected_ensure_failures_retry_per_upload() -> Result<()> {
        harnx_core::require_nextest();
        let Some(nats) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(());
        };
        let (_sandbox, server) = upload_server_for_cluster_yaml(
            "ensurefail",
            &format!("url: {}\nreplicas: 2\n", nats.url()),
        )
        .await?;

        let (status, json) = post_upload(
            Arc::clone(&server),
            "/v1/agents/plain/sessions/ensurefail-1/attachments",
            &png_magic_bytes(),
        )
        .await?;
        assert!(status.is_server_error(), "first upload fails: {json}");
        assert_no_upload_refs(&json);
        assert_eq!(server.upload_storage_counts("ensurefail").await, (1, 1));

        // A live connection is never deferred: the store-level failure
        // fails fast, so each upload retries the ensure.
        let (status, json) = post_upload(
            Arc::clone(&server),
            "/v1/agents/plain/sessions/ensurefail-2/attachments",
            &png_magic_bytes(),
        )
        .await?;
        assert!(status.is_server_error(), "second upload fails: {json}");
        assert_no_upload_refs(&json);
        assert_eq!(
            server.upload_storage_counts("ensurefail").await,
            (1, 2),
            "a connected broker keeps the per-upload ensure retry"
        );
        Ok(())
    }

    /// A failed bucket ensure defers its retry only while the connection is
    /// down (D2): during an outage no upload re-ensures under the
    /// single-flight lock (C7), and once the broker is back the next upload
    /// retries immediately — no recheck interval can hold recovery back.
    #[tokio::test]
    async fn upload_attachments_ensure_failure_defers_during_outage_and_recovers() -> Result<()> {
        harnx_core::require_nextest();
        let Some(mut nats) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(());
        };
        let (_sandbox, server) = upload_server_for_cluster("ensureout", nats.url()).await?;

        // Warm the cache: the first upload connects and ensures.
        let (status, json) = post_upload(
            Arc::clone(&server),
            "/v1/agents/plain/sessions/ensureout-1/attachments",
            &png_magic_bytes(),
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(server.upload_storage_counts("ensureout").await, (1, 1));

        // Delete the object-store stream: the next upload's put fails, and
        // the missing-stream probe resets the bucket memo — the memo-unset
        // state the down-connection deferral governs.
        let port = nats
            .url()
            .rsplit(':')
            .next()
            .context("parse broker port")?
            .parse::<u16>()
            .context("parse broker port")?;
        let jetstream = async_nats::jetstream::new(async_nats::connect(nats.url()).await?);
        jetstream
            .delete_stream(harnx_blob_store::media::attachments_stream_name())
            .await?;
        let (status, json) = post_upload(
            Arc::clone(&server),
            "/v1/agents/plain/sessions/ensureout-2/attachments",
            &png_magic_bytes(),
        )
        .await?;
        assert!(
            status.is_server_error(),
            "the deleted stream fails the put: {json}"
        );
        assert_eq!(
            server.upload_storage_counts("ensureout").await,
            (1, 1),
            "the failed put resets the memo without a new ensure"
        );

        // Outage: the broker dies. While the connection is down, uploads
        // defer the ensure retry instead of stampeding the single-flight
        // lock with attempts that can only queue until the request timeout
        // (C7) — both fail instantly, without a new ensure attempt.
        nats.kill().expect("kill broker");
        nats.wait().expect("reap broker");
        // The cached connection notices the dead broker asynchronously;
        // poll it (bounded) so the deferral check below is deterministic
        // instead of racing a blind settle delay.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while server.upload_connection_state("ensureout").await
            == Some(async_nats::connection::State::Connected)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the cached connection must notice the dead broker"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        for session in ["ensureout-3", "ensureout-4"] {
            let (status, json) = post_upload(
                Arc::clone(&server),
                &format!("/v1/agents/plain/sessions/{session}/attachments"),
                &png_magic_bytes(),
            )
            .await?;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
            assert_no_upload_refs(&json);
            assert_eq!(
                server.upload_storage_counts("ensureout").await,
                (1, 1),
                "a down connection must defer the ensure retry, not attempt it"
            );
        }

        // Recovery: the broker returns on the same port. The next upload
        // must succeed as soon as the connection is back — the 10-minute
        // success-side recheck interval never governs the retry. Poll with a
        // hard deadline so a broken recovery fails instead of hanging.
        let Some(_revived) = harnx_test_bins::spawn_nats_server_on_port(port).await? else {
            return Ok(());
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut recovered = None;
        while std::time::Instant::now() < deadline {
            let (status, json) = post_upload(
                Arc::clone(&server),
                "/v1/agents/plain/sessions/ensureout-5/attachments",
                &png_magic_bytes(),
            )
            .await?;
            if status == StatusCode::OK {
                recovered = Some(json);
                break;
            }
            assert!(
                status.is_server_error(),
                "interim failure must be 5xx: {json}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let json = recovered.context("the upload must recover once the broker returns")?;
        assert!(
            json.get("attachment_refs").is_some(),
            "the recovered upload returns refs: {json}"
        );
        assert_eq!(
            server.upload_storage_counts("ensureout").await,
            (1, 2),
            "the recovery re-ensured the bucket exactly once"
        );
        Ok(())
    }

    /// The session-metadata reads on the upload path never provision the
    /// metadata bucket (D7's read-only invariant): a never-created session
    /// still uploads, and the bucket is still absent afterwards.
    #[tokio::test]
    async fn upload_attachments_never_provisions_the_metadata_bucket() -> Result<()> {
        harnx_core::require_nextest();
        let Some(nats) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(());
        };
        let (_sandbox, server) = upload_server_for_cluster("noprov", nats.url()).await?;

        let (status, json) = post_upload(
            Arc::clone(&server),
            "/v1/agents/plain/sessions/noprov-1/attachments",
            &png_magic_bytes(),
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "D7 session uploads: {json}");

        let jetstream = async_nats::jetstream::new(async_nats::connect(nats.url()).await?);
        let error = jetstream
            .get_key_value(harnx_runtime::nats_session_metadata::SESSION_METADATA_BUCKET)
            .await
            .err()
            .context("the metadata bucket must not exist after an upload")?;
        assert!(
            harnx_runtime::nats_admin::kv_bucket_missing(&error),
            "the read-only capture must not create the bucket: {error:#}"
        );
        Ok(())
    }

    /// The connection-state classification of a failed bucket ensure,
    /// without a broker: every non-connected state is unavailability.
    #[test]
    fn upload_ensure_failure_classifies_by_connection_state() {
        use async_nats::connection::State;
        for state in [State::Pending, State::Disconnected] {
            let name = format!("{state:?}");
            let classified = Server::classify_upload_ensure_failure(
                state,
                "cluster",
                anyhow::anyhow!("create the attachments bucket"),
            );
            assert!(
                crate::nats_access::is_nats_unavailable(&classified),
                "{name} must classify as unavailability"
            );
        }
        // A live connection keeps the store-level failure for the wrapper
        // kinds to classify: 500 when the broker answered.
        let connected = Server::classify_upload_ensure_failure(
            State::Connected,
            "cluster",
            anyhow::anyhow!("create the attachments bucket"),
        );
        assert!(
            !crate::nats_access::is_nats_unavailable(&connected),
            "a connected broker's failure stays a store failure"
        );
        assert_eq!(
            connected.to_string(),
            "create the attachments bucket",
            "the original chain is preserved for a connected broker"
        );
    }

    /// Isolated broker + rules-on upload server: the preamble the M2 rules-on
    /// tests share. `None` skips the test the same way `spawn_nats_server`
    /// does when the broker binary is unavailable. The sandbox stays alive:
    /// the cluster's NATS yaml is read at request time, not at startup.
    async fn m2_rules_on_fixture(
        cluster: &str,
    ) -> Result<
        Option<(
            harnx_test_bins::NatsServerHandle,
            TestConfigSandbox,
            std::sync::Arc<Server>,
        )>,
    > {
        m2_rules_on_fixture_for(cluster, "alice").await
    }

    /// As [`m2_rules_on_fixture`], with a custom rule users list, so a
    /// test can admit several callers and separate them by session
    /// ownership rather than agent visibility.
    async fn m2_rules_on_fixture_for(
        cluster: &str,
        users: &str,
    ) -> Result<
        Option<(
            harnx_test_bins::NatsServerHandle,
            TestConfigSandbox,
            std::sync::Arc<Server>,
        )>,
    > {
        harnx_core::require_nextest();
        let Some(nats) = harnx_test_bins::spawn_nats_server().await? else {
            return Ok(None);
        };
        let (sandbox, server) =
            upload_server_with_access_rules_for(cluster, nats.url(), users).await?;
        Ok(Some((nats, sandbox, server)))
    }

    /// A record whose properties do not decode cannot prove anyone's
    /// authorization: the proof answers None (404), never a 500, and the
    /// decode failure is logged.
    #[test]
    fn proof_from_record_fails_closed_on_undecodable_properties() -> Result<()> {
        let rules = harnx_core::access_rules::AccessRules::from_yaml(
            "rules:\n  - agents: ['*']\n    users: [alice]\n",
        )
        .context("parse test access rules")?;
        let mut record = harnx_runtime::nats_session_metadata::MetadataRecord {
            metadata: harnx_runtime::nats_session_metadata::SessionMetadata::new(
                "undecodable1",
                harnx_runtime::nats_session_metadata::SessionInitializer::named(
                    "plain",
                    Default::default(),
                ),
            ),
            revision: 1,
        };
        // Properties that cannot deserialize: the namespace extension holds
        // a plain string.
        record.metadata.extensions.insert(
            harnx_runtime::nats_session_metadata::SESSION_PROPERTIES_NAMESPACE.to_string(),
            Value::String("not session properties".to_string()),
        );
        let caller = harnx_core::access_rules::CallerView::from_users(&["alice"]);
        assert!(
            Server::proof_from_record(&rules, "plain", caller, &record).is_none(),
            "an undecodable record denies rather than 500ing"
        );
        Ok(())
    }

    /// With access rules on, a session that was deleted after the guard
    /// proved it and before the first part is a 404, not a D7-style 200:
    /// the route's incarnation proof turns absence into "gone" (M2), and
    /// nothing is stored.
    /// Direct route call, no guard: the route's own incarnation proof must
    /// answer 404 for a session whose record is already gone. (Through
    /// dispatch the guard answers that case itself — see the dispatch guard
    /// test, which deletes the record before the request. A deletion
    /// between the guard's authorization and this proof has no
    /// deterministic seam; this test pins the route's own absence check.)
    #[tokio::test]
    async fn upload_route_proof_404s_deleted_session_direct_handler() -> Result<()> {
        let Some((nats, _sandbox, server)) = m2_rules_on_fixture("m2gone").await? else {
            return Ok(());
        };
        f6_create_session_metadata_as(nats.url(), "m2gone1", Some("alice")).await?;
        f6_delete_session_metadata(nats.url(), "m2gone1").await?;

        let (status, json) = rules_on_png_upload(&server, "m2gone1").await?;
        assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
        assert_eq!(
            json.get("error").and_then(Value::as_str),
            Some("session no longer exists"),
            "{json}"
        );
        assert_no_upload_refs(&json);

        // Nothing was stored: the 404 fires before the first part.
        let data = png_magic_bytes();
        let url = f6_media_url("m2gone1", &data)?;
        let bucket = f6_attachments_bucket(nats.url()).await?;
        let info = harnx_blob_store::media::leader_media_info(&bucket, &url.kv_key()).await?;
        assert!(info.is_none(), "no object version for the deleted session");
        Ok(())
    }

    /// The rules-on incarnation proof must not break the live-session
    /// happy path: a session that still exists uploads normally.
    #[tokio::test]
    async fn upload_attachments_live_session_succeeds_with_access_rules_on() -> Result<()> {
        let Some((nats, _sandbox, server)) = m2_rules_on_fixture("m2live").await? else {
            return Ok(());
        };
        f6_create_session_metadata_as(nats.url(), "m2live1", Some("alice")).await?;

        let (status, json) = rules_on_png_upload(&server, "m2live1").await?;
        assert_eq!(status, StatusCode::OK, "{json}");
        let refs = json
            .get("attachment_refs")
            .and_then(Value::as_array)
            .context("successful upload must return refs")?;
        assert_eq!(refs.len(), 1, "{json}");
        Ok(())
    }

    /// A session replaced between the guard's authorization and the route's
    /// proof must not be uploadable by the caller the guard approved: the
    /// proof pins the incarnation only of a record the caller can still
    /// access (the guard's own policy), so a replacement owned by another
    /// user answers 404 before anything is stored. The replacement's own
    /// owner still uploads normally.
    /// Direct route call, no guard: the proof's own authorization check
    /// must reject a record the guard never saw. Through dispatch the guard
    /// rejects this caller first (see the dispatch guard test); this pins
    /// the route's defense independently.
    #[tokio::test]
    async fn upload_route_proof_404s_replacement_other_owner_direct_handler() -> Result<()> {
        // Both alice and bob appear in the rules: the swap's whole point is
        // that bob is a legitimate caller for the replacement record.
        let Some((nats, _sandbox, server)) =
            m2_rules_on_fixture_for("m2swap", "alice, bob").await?
        else {
            return Ok(());
        };
        f6_create_session_metadata_as(nats.url(), "m2swap1", Some("alice")).await?;
        f6_delete_session_metadata(nats.url(), "m2swap1").await?;
        // Recreated under the same id with a different owner.
        f6_create_session_metadata_as(nats.url(), "m2swap1", Some("bob")).await?;

        let (status, json) = rules_on_png_upload_as(&server, "m2swap1", "alice").await?;
        assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
        assert_eq!(
            json.get("error").and_then(Value::as_str),
            Some("session no longer exists"),
            "{json}"
        );
        assert_no_upload_refs(&json);
        let data = png_magic_bytes();
        let url = f6_media_url("m2swap1", &data)?;
        let bucket = f6_attachments_bucket(nats.url()).await?;
        let info = harnx_blob_store::media::leader_media_info(&bucket, &url.kv_key()).await?;
        assert!(
            info.is_none(),
            "nothing is stored for a session the caller cannot access"
        );

        // The replacement's own owner uploads to it as usual.
        let (status, json) = rules_on_png_upload_as(&server, "m2swap1", "bob").await?;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert!(
            json.get("attachment_refs").is_some(),
            "the authorized owner's upload returns refs: {json}"
        );
        Ok(())
    }

    /// Through the real HTTP path the guard runs before the upload route:
    /// the session's owner uploads successfully, a caller who can see the
    /// agent but does not own the session is denied by the guard's
    /// session-ownership check, and so is an anonymous caller.
    #[tokio::test]
    async fn upload_attachments_dispatch_guard_authorizes_owner_and_denies_others() -> Result<()> {
        // Both alice and bob are in the rules, so both pass the guard's
        // agent-visibility check; ownership is what separates them below.
        let Some((nats, _sandbox, server)) =
            m2_rules_on_fixture_for("m2guard", "alice, bob").await?
        else {
            return Ok(());
        };
        f6_create_session_metadata_as(nats.url(), "m2guard1", Some("alice")).await?;

        let (status, json) = rules_on_upload_via_http(&server, "m2guard1", Some("alice")).await?;
        assert_eq!(status, StatusCode::OK.as_u16(), "{json}");
        assert!(
            json.get("attachment_refs").is_some(),
            "the owner's guarded upload returns refs: {json}"
        );

        // Bob passes agent visibility (his own session below uploads fine),
        // so the 404 on alice's session comes from the guard's
        // session-ownership check, not agent invisibility.
        f6_create_session_metadata_as(nats.url(), "m2guard2", Some("bob")).await?;
        let (status, json) = rules_on_upload_via_http(&server, "m2guard2", Some("bob")).await?;
        assert_eq!(status, StatusCode::OK.as_u16(), "{json}");
        assert!(
            json.get("attachment_refs").is_some(),
            "bob's own session proves he sees the agent: {json}"
        );

        let (status, json) = rules_on_upload_via_http(&server, "m2guard1", Some("bob")).await?;
        assert_eq!(status, StatusCode::NOT_FOUND.as_u16(), "{json}");
        assert_no_upload_refs(&json);
        assert_eq!(
            json.get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str),
            Some("Not Found"),
            "the guard's own denial answers, not the route's proof fallback: {json}"
        );

        // An anonymous request never reaches the guard: with rules on, the
        // preflight identity gate rejects protected routes outright (401).
        let (status, json) = rules_on_upload_via_http(&server, "m2guard1", None).await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED.as_u16(), "{json}");
        assert_no_upload_refs(&json);

        // A session deleted before the request is the guard's own 404: the
        // owner never reaches the route's proof. The stored object from the
        // first upload stays — its content-addressed key is not this
        // request's to purge — so only the response is asserted here.
        f6_delete_session_metadata(nats.url(), "m2guard1").await?;
        let (status, json) = rules_on_upload_via_http(&server, "m2guard1", Some("alice")).await?;
        assert_eq!(status, StatusCode::NOT_FOUND.as_u16(), "{json}");
        assert_no_upload_refs(&json);
        Ok(())
    }

    /// The guard's broker-unavailability failures surface as 503, not the
    /// generic 400: with rules on, a rules-authorized upload to an
    /// unreachable cluster fails the guard's metadata open before the
    /// upload route's own classification runs.
    #[tokio::test]
    async fn upload_attachments_guard_unreachable_cluster_is_503() -> Result<()> {
        harnx_core::require_nextest();
        let (_sandbox, server) =
            upload_server_with_access_rules("downguard", "nats://127.0.0.1:1").await?;

        let (status, json) = rules_on_upload_via_http(&server, "downguard1", Some("alice")).await?;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE.as_u16(), "{json}");
        assert_no_upload_refs(&json);
        Ok(())
    }

    // ===== Web-ui static asset serving tests (#1006) =====

    fn asset_server(assets_dir: PathBuf) -> Server {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        // Server::new snapshots the config at construction, so the sandbox can
        // be dropped afterwards without affecting the built Server.
        let config = Arc::new(ConfigLock::new(sandbox.config()));
        Server::new(&config, assets_dir)
    }

    #[tokio::test]
    async fn html_pages_escape_request_derived_agent_and_session_values() {
        let server = asset_server(PathBuf::from("web-assets"));
        let markup = "<img src=x onerror=alert(1)>";

        let session = server
            .session_html_page("plain", markup)
            .expect("render session page");
        assert_eq!(
            asset_body(session).await,
            b"<h1>session: &lt;img src=x onerror=alert(1)&gt;</h1>"
        );

        let agent = server.agent_html_page(markup).expect("render agent page");
        assert_eq!(
            asset_body(agent).await,
            b"<h1>agent: &lt;img src=x onerror=alert(1)&gt;</h1>"
        );

        // Review D5: the C2 regression guard covers all five
        // HTML-significant characters, not only the angle brackets.
        let quotes = "&\"'";
        let session = server
            .session_html_page("plain", quotes)
            .expect("render session page");
        assert_eq!(
            asset_body(session).await,
            b"<h1>session: &amp;&quot;&#39;</h1>"
        );
        let agent = server.agent_html_page(quotes).expect("render agent page");
        assert_eq!(asset_body(agent).await, b"<h1>agent: &amp;&quot;&#39;</h1>");
    }
    async fn asset_body(response: AppResponse) -> Vec<u8> {
        response
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes()
            .to_vec()
    }

    #[tokio::test]
    async fn serve_web_asset_returns_file_with_content_type() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("app.js"), b"console.log('hi');").expect("write asset");
        let server = asset_server(dir.path().to_path_buf());

        let response = server
            .serve_web_asset(&Method::GET, "/app.js", &http::HeaderMap::new())
            .await
            .expect("serve asset");

        assert_eq!(
            response
                .headers()
                .get("Content-Type")
                .and_then(|v| v.to_str().ok()),
            Some("application/javascript; charset=utf-8")
        );
        assert_eq!(asset_body(response).await, b"console.log('hi');");
    }

    #[tokio::test]
    async fn serve_web_asset_root_serves_index_html() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("index.html"), b"<h1>home</h1>").expect("write index");
        let server = asset_server(dir.path().to_path_buf());

        let response = server
            .serve_web_asset(&Method::GET, "/", &http::HeaderMap::new())
            .await
            .expect("serve index");

        assert_eq!(
            response
                .headers()
                .get("Content-Type")
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(asset_body(response).await, b"<h1>home</h1>");
    }

    #[tokio::test]
    async fn serve_web_asset_rejects_path_traversal() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("index.html"), b"ok").expect("write index");
        // A secret file that lives OUTSIDE the assets root.
        let secret = dir.path().parent().expect("parent").join("secret.txt");
        std::fs::write(&secret, b"top secret").expect("write secret");
        let server = asset_server(dir.path().to_path_buf());

        let err = server
            .serve_web_asset(&Method::GET, "/../secret.txt", &http::HeaderMap::new())
            .await
            .expect_err("traversal must be rejected");
        assert_eq!(err.to_string(), "Not Found");
        assert_eq!(status_from_error(&err), Some(StatusCode::NOT_FOUND));

        let _ = std::fs::remove_file(&secret);
    }

    #[tokio::test]
    async fn serve_web_asset_missing_file_returns_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = asset_server(dir.path().to_path_buf());

        let err = server
            .serve_web_asset(&Method::GET, "/missing.css", &http::HeaderMap::new())
            .await
            .expect_err("missing file should 404");
        assert_eq!(status_from_error(&err), Some(StatusCode::NOT_FOUND));
    }

    #[tokio::test]
    async fn serve_web_asset_missing_root_returns_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing_root = dir.path().join("does-not-exist");
        let server = asset_server(missing_root);

        let err = server
            .serve_web_asset(&Method::GET, "/index.html", &http::HeaderMap::new())
            .await
            .expect_err("missing root should 404");
        assert_eq!(status_from_error(&err), Some(StatusCode::NOT_FOUND));
    }

    #[tokio::test]
    async fn serve_web_asset_spa_fallback_for_navigation() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("index.html"), b"<h1>spa</h1>").expect("write index");
        let server = asset_server(dir.path().to_path_buf());

        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::ACCEPT, HeaderValue::from_static("text/html"));

        // Unknown extensionless route + Accept: text/html => index.html.
        let response = server
            .serve_web_asset(&Method::GET, "/dashboard/settings", &headers)
            .await
            .expect("spa fallback");
        assert_eq!(asset_body(response).await, b"<h1>spa</h1>");
    }

    #[tokio::test]
    async fn serve_web_asset_percent_decodes_filename() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("my file.txt"), b"spaced").expect("write asset");
        let server = asset_server(dir.path().to_path_buf());

        let response = server
            .serve_web_asset(&Method::GET, "/my%20file.txt", &http::HeaderMap::new())
            .await
            .expect("serve percent-encoded asset");
        assert_eq!(asset_body(response).await, b"spaced");
    }

    #[tokio::test]
    async fn serve_web_asset_sets_content_length() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("app.js"), b"payload").expect("write asset");
        let server = asset_server(dir.path().to_path_buf());

        let response = server
            .serve_web_asset(&Method::GET, "/app.js", &http::HeaderMap::new())
            .await
            .expect("serve asset");
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            Some("7")
        );

        // HEAD advertises the same length with an empty body.
        let head = server
            .serve_web_asset(&Method::HEAD, "/app.js", &http::HeaderMap::new())
            .await
            .expect("serve head");
        assert_eq!(
            head.headers()
                .get(http::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            Some("7")
        );
        assert!(asset_body(head).await.is_empty());
    }

    #[tokio::test]
    async fn serve_web_asset_head_omits_body() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("app.js"), b"payload").expect("write asset");
        let server = asset_server(dir.path().to_path_buf());

        let response = server
            .serve_web_asset(&Method::HEAD, "/app.js", &http::HeaderMap::new())
            .await
            .expect("serve head");
        assert_eq!(
            response
                .headers()
                .get("Content-Type")
                .and_then(|v| v.to_str().ok()),
            Some("application/javascript; charset=utf-8")
        );
        assert!(asset_body(response).await.is_empty());
    }

    #[test]
    fn sanitize_asset_path_blocks_traversal() {
        assert!(sanitize_asset_path("../etc/passwd").is_none());
        assert!(sanitize_asset_path("a/../../b").is_none());
        assert_eq!(
            sanitize_asset_path("assets/app.js"),
            Some(PathBuf::from("assets/app.js"))
        );
        assert_eq!(
            sanitize_asset_path("/assets/app.js"),
            Some(PathBuf::from("assets/app.js"))
        );
        assert_eq!(sanitize_asset_path(""), Some(PathBuf::new()));
    }

    #[test]
    fn content_type_for_path_covers_common_extensions() {
        assert_eq!(
            content_type_for_path(Path::new("x.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type_for_path(Path::new("x.css")),
            "text/css; charset=utf-8"
        );
        assert_eq!(content_type_for_path(Path::new("x.png")), "image/png");
        assert_eq!(
            content_type_for_path(Path::new("x.wasm")),
            "application/wasm"
        );
        assert_eq!(
            content_type_for_path(Path::new("x.unknown")),
            "application/octet-stream"
        );
    }

    // -------------------------------------------------------------------------
    // Tests for default-cluster display normalization
    // -------------------------------------------------------------------------

    /// When running in cluster-client mode, agents on the default cluster
    /// should be listed without the `@<cluster>` suffix.
    #[tokio::test]
    async fn list_agents_hides_suffix_for_default_cluster() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent_with_front_matter(
            "assistant-alpha",
            "role: assistant\nmodel: openai:gpt-4o\ndescription: Local assistant",
            "You are assistant alpha.",
        );
        sandbox.write_nats_server(
            "mycluster",
            "url: nats://localhost:4222\nagents:\n  - name: sisyphus\n    description: Remote assistant\n    role: assistant\n  - name: remote-helper\n    role: subagent\n",
        );

        // Configure server in cluster-client mode
        let mut config = sandbox.config();
        config.nats_routing = NatsRouting::Cluster("mycluster".to_string());
        let config = Arc::new(ConfigLock::new(config));
        let server = Server::new(&config, std::path::PathBuf::from("web-assets"));

        let agents = response_json(
            server
                .list_agents(&agents_request(None))
                .await
                .expect("agents"),
        )
        .await;
        let names = agent_names(&agents);

        // Default cluster agent should appear without suffix, local with bare name,
        // remote helper without suffix since it's on the default cluster.
        assert_eq!(
            names.iter().filter(|&n| n == "sisyphus").count(),
            1,
            "expected 'sisyphus' without suffix, got {names:?}"
        );
        assert!(
            names.contains(&"assistant-alpha".to_string()),
            "expected 'assistant-alpha', got {names:?}"
        );
        assert!(
            !names.contains(&"remote-helper@mycluster".to_string()),
            "expected no 'remote-helper@mycluster' suffix, got {names:?}"
        );
        assert!(
            names.contains(&"remote-helper".to_string()),
            "expected 'remote-helper' without suffix, got {names:?}"
        );
    }

    /// When NOT in cluster-client mode, agents should keep their @cluster suffix.
    #[tokio::test]
    async fn list_agents_keeps_suffix_without_default_cluster() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent_with_front_matter(
            "assistant-alpha",
            "role: assistant\nmodel: openai:gpt-4o\ndescription: Local assistant",
            "You are assistant alpha.",
        );
        sandbox.write_nats_server(
            "shared",
            "url: nats://localhost:4222\nagents:\n  - name: sisyphus\n    description: Remote assistant\n    role: assistant\n",
        );

        // Default config (no HARNX_NATS_SERVER) uses NatsRouting::Default
        let config = Arc::new(ConfigLock::new(sandbox.config()));
        let server = Server::new(&config, std::path::PathBuf::from("web-assets"));

        let agents = response_json(
            server
                .list_agents(&agents_request(None))
                .await
                .expect("agents"),
        )
        .await;
        let names = agent_names(&agents);

        // Remote agent should appear WITH suffix, local with bare name
        assert!(
            names.contains(&"sisyphus@shared".to_string()),
            "expected 'sisyphus@shared' with suffix, got {names:?}"
        );
        assert!(
            names.contains(&"assistant-alpha".to_string()),
            "expected 'assistant-alpha', got {names:?}"
        );
    }

    /// Agents on non-default clusters should keep their suffix even in cluster-client mode.
    #[tokio::test]
    async fn list_agents_keeps_suffix_for_non_default_cluster() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_nats_server(
            "mycluster",
            "url: nats://localhost:4222\nagents:\n  - name: sisyphus\n    description: Default cluster agent\n    role: assistant\n",
        );
        sandbox.write_nats_server(
            "other",
            "url: nats://localhost:4222\nagents:\n  - name: atlas\n    description: Other cluster agent\n    role: assistant\n",
        );

        // Configure server in cluster-client mode with 'mycluster' as default
        let mut config = sandbox.config();
        config.nats_routing = NatsRouting::Cluster("mycluster".to_string());
        let config = Arc::new(ConfigLock::new(config));
        let server = Server::new(&config, std::path::PathBuf::from("web-assets"));

        let agents = response_json(
            server
                .list_agents(&agents_request(None))
                .await
                .expect("agents"),
        )
        .await;
        let names = agent_names(&agents);

        // Default cluster agent without suffix, non-default with suffix
        assert!(
            names.contains(&"sisyphus".to_string()),
            "expected 'sisyphus' without suffix, got {names:?}"
        );
        assert!(
            names.contains(&"atlas@other".to_string()),
            "expected 'atlas@other' with suffix, got {names:?}"
        );
    }

    /// Test deduplication: bare name and @cluster name should merge for default cluster.
    #[tokio::test]
    async fn list_agents_dedupes_bare_and_suffixed_for_default_cluster() {
        let sandbox = TestConfigSandbox::new();
        // Write a local agent
        sandbox.write_agent_with_front_matter(
            "sisyphus",
            "role: assistant\nmodel: openai:gpt-4o\ndescription: Local sisyphus",
            "You are sisyphus.",
        );
        // Write a remote agent with the same name on the default cluster
        sandbox.write_nats_server(
            "mycluster",
            "url: nats://localhost:4222\nagents:\n  - name: sisyphus\n    description: Remote sisyphus\n    role: assistant\n",
        );

        // Configure server in cluster-client mode
        let mut config = sandbox.config();
        config.nats_routing = NatsRouting::Cluster("mycluster".to_string());
        let config = Arc::new(ConfigLock::new(config));
        let server = Server::new(&config, std::path::PathBuf::from("web-assets"));

        let agents = response_json(
            server
                .list_agents(&agents_request(None))
                .await
                .expect("agents"),
        )
        .await;
        let agents_list = agents["data"].as_array().expect("agents array");

        // Should have exactly one "sisyphus" entry (deduped), not two
        let sisyphus_count = agents_list
            .iter()
            .filter(|a| a["name"].as_str() == Some("sisyphus"))
            .count();
        assert_eq!(
            sisyphus_count, 1,
            "expected exactly 1 'sisyphus' entry after dedup, got {agents_list:?}"
        );

        // The remote agent's description should win (bare names route to default cluster)
        let sisyphus_entry = agents_list
            .iter()
            .find(|a| a["name"].as_str() == Some("sisyphus"))
            .expect("sisyphus entry should exist");
        assert_eq!(
            sisyphus_entry["description"].as_str(),
            Some("Remote sisyphus"),
            "expected remote agent's description to win, got {sisyphus_entry:?}"
        );
    }

    /// A described local agent must not displace an undescribed default-cluster
    /// remote with the same name: the bare name routes to the remote.
    #[tokio::test]
    async fn list_agents_dedup_prefers_default_remote_even_without_description() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent_with_front_matter(
            "sisyphus",
            "role: assistant\nmodel: openai:gpt-4o\ndescription: Local sisyphus",
            "You are sisyphus.",
        );
        sandbox.write_nats_server(
            "mycluster",
            "url: nats://localhost:4222\nagents:\n  - name: sisyphus\n    role: assistant\n",
        );

        let mut config = sandbox.config();
        config.nats_routing = NatsRouting::Cluster("mycluster".to_string());
        let config = Arc::new(ConfigLock::new(config));
        let server = Server::new(&config, std::path::PathBuf::from("web-assets"));

        let agents = response_json(
            server
                .list_agents(&agents_request(None))
                .await
                .expect("agents"),
        )
        .await;
        let matches: Vec<_> = agents["data"]
            .as_array()
            .expect("agents array")
            .iter()
            .filter(|a| a["name"].as_str() == Some("sisyphus"))
            .collect();
        assert_eq!(matches.len(), 1, "expected one sisyphus entry: {agents:?}");
        assert_ne!(
            matches[0]["description"].as_str(),
            Some("Local sisyphus"),
            "local agent must not win the collision: {:?}",
            matches[0]
        );
    }

    /// Test role=assistant filter works in cluster-client mode (regression test).
    /// Remote assistants on the default cluster must not be dropped.
    #[tokio::test]
    async fn list_agents_filters_assistants_in_cluster_mode() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent_with_front_matter(
            "assistant-alpha",
            "role: assistant\nmodel: openai:gpt-4o\ndescription: Local assistant",
            "You are assistant alpha.",
        );
        sandbox.write_agent_with_front_matter(
            "helper-beta",
            "role: subagent\nmodel: openai:gpt-4o\ndescription: Local helper",
            "You are helper beta.",
        );
        sandbox.write_nats_server(
            "mycluster",
            "url: nats://localhost:4222\nagents:\n  - name: sisyphus\n    description: Remote assistant\n    role: assistant\n  - name: remote-helper\n    role: subagent\n",
        );

        // Configure server in cluster-client mode
        let mut config = sandbox.config();
        config.nats_routing = NatsRouting::Cluster("mycluster".to_string());
        let config = Arc::new(ConfigLock::new(config));
        let server = Server::new(&config, std::path::PathBuf::from("web-assets"));

        // Test role=assistant filter
        let filtered = response_json(
            server
                .list_agents(&agents_request(Some("role=assistant")))
                .await
                .expect("assistant agents"),
        )
        .await;
        let names = agent_names(&filtered);

        // Should have exactly 2 names: local assistant-alpha and remote sisyphus
        // (remote sisyphus appears as bare "sisyphus" since it's on the default cluster)
        assert_eq!(names.len(), 2, "expected 2 assistants, got {names:?}");
        assert!(
            names.contains(&"assistant-alpha".to_string()),
            "expected 'assistant-alpha', got {names:?}"
        );
        assert!(
            names.contains(&"sisyphus".to_string()),
            "expected 'sisyphus' (bare), got {names:?}"
        );

        // Sub-agents should NOT be in the filtered list
        let excluded = ["helper-beta", "remote-helper", "remote-helper@mycluster"];
        assert!(
            excluded.iter().all(|n| !names.contains(&n.to_string())),
            "sub-agents should not appear: expected none of {excluded:?}, got {names:?}"
        );
    }

    /// Test round-trip: bare name from list should resolve back to the same agent.
    #[tokio::test]
    async fn bare_name_round_trips_to_default_cluster_agent() {
        use crate::agent_resolve::resolve_agent_target;

        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent_with_front_matter(
            "sisyphus",
            "role: assistant\nmodel: openai:gpt-4o\ndescription: Local sisyphus",
            "You are sisyphus.",
        );
        sandbox.write_nats_server(
            "mycluster",
            "url: nats://localhost:4222\nagents:\n  - name: atlas\n    description: Remote atlas\n    role: assistant\n",
        );

        // Configure server in cluster-client mode
        let mut config = sandbox.config();
        config.nats_routing = NatsRouting::Cluster("mycluster".to_string());

        // Bare name should resolve to the default cluster
        let (target, _) = resolve_agent_target(&config, "atlas")
            .await
            .expect("bare atlas should resolve");
        assert_eq!(target.agent(), "atlas");
        assert_eq!(target.cluster(), "mycluster");

        // Bare name for local agent should still work (routes to default, but local exists)
        let (local_target, _) = resolve_agent_target(&config, "sisyphus")
            .await
            .expect("bare sisyphus should resolve");
        assert_eq!(local_target.agent(), "sisyphus");
        assert_eq!(local_target.cluster(), "mycluster");

        // Old-style suffixed name should still work
        let (suffixed_target, _) = resolve_agent_target(&config, "atlas@mycluster")
            .await
            .expect("atlas@mycluster should resolve");
        assert_eq!(suffixed_target.agent(), "atlas");
        assert_eq!(suffixed_target.cluster(), "mycluster");
    }

    fn user_prompt_row(id: &str) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: Some(id.to_string()),
            role: harnx_core::message::MessageRole::User,
            content: MessageContent::Text("original".into()),
            timestamp: None,
            fence_token: None,
        }
    }

    fn edit_row(seq: u64, from: usize, to: usize) -> (u64, SessionLogEntry) {
        (
            seq,
            SessionLogEntry::EditEntries {
                from,
                to,
                replacements: vec![],
            },
        )
    }

    /// The prompt at seq 1 followed by `count` chained edits: the edit at
    /// seq i+1 rewrites the row at seq i — the prompt for the first edit,
    /// the previous edit's mutation row for every one after that. Tracing
    /// the prompt's age from the last edit's range descends once per edit.
    fn edit_chain(count: usize) -> Vec<(u64, SessionLogEntry)> {
        let mut entries = vec![(1, user_prompt_row("original-prompt"))];
        for i in 1..=count {
            entries.push(edit_row(i as u64 + 1, i, i));
        }
        entries
    }

    #[test]
    fn edit_chain_trace_resolves_a_chain_within_the_depth_cap() {
        let entries = edit_chain(MAX_EDIT_TRACE_DEPTH);
        let row = original_prompt_row(&entries, MAX_EDIT_TRACE_DEPTH, MAX_EDIT_TRACE_DEPTH)
            .expect("a chain within the cap resolves to the original prompt");
        assert!(matches!(row, SessionLogEntry::Message { id, role, .. }
                if id.as_deref() == Some("original-prompt") && role.is_user()));
    }

    #[test]
    fn edit_chain_trace_gives_up_one_hop_past_the_depth_cap() {
        // One more chained edit than the cap allows: the trace cannot reach
        // the original prompt, and a bounded miss reads as no traceable age.
        let entries = edit_chain(MAX_EDIT_TRACE_DEPTH + 1);
        assert_eq!(
            original_prompt_row(&entries, MAX_EDIT_TRACE_DEPTH + 1, MAX_EDIT_TRACE_DEPTH + 1),
            None
        );
    }

    #[test]
    fn cyclic_edit_ranges_terminate_at_the_depth_cap() {
        // Two edits whose ranges point at each other: without the cap the
        // trace would recurse forever; with it, the bounded miss reads as no
        // traceable age.
        let entries = [edit_row(100, 101, 101), edit_row(101, 100, 100)];
        assert_eq!(original_prompt_row(&entries, 101, 101), None);
    }
}
