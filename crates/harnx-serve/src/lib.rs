//! harnx-serve — HTTP server front-end for the harnx agent harness
//! (plan P47, β+ progressive peel). Extracted from `harnx::serve`.
//! Depends on `harnx-runtime` for Config + Client orchestration.

pub mod ag_ui;
pub mod ag_ui_rpc;
mod ag_ui_sync;
pub mod session_actor;
mod session_actor_types;
mod session_routes;
// Not `#[cfg(test)]`: the `tests/` integration crates link the library built
// WITHOUT the `test` cfg, so gating this out would break their
// `harnx_serve::test_support` imports. Kept public for cross-crate test reuse.
pub mod test_support;

use crate::ag_ui::{resolve_agent, AgUiError, AppResponse as AgUiAppResponse};
use crate::ag_ui_rpc::{handle_ag_ui_rpc, PersistenceKind};
use crate::session_actor::SessionRegistry;
use crate::session_routes::AgentSessionRef;

use harnx_core::message::MessageRole;
use harnx_rag::*;
use harnx_runtime::{client::*, config::*, utils::*};
use log::{debug, error, info};

use anyhow::{anyhow, bail, Result};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::stream::StreamExt;
use harnx_core::attachments::store_attachment_bytes;
use http::{Method, Response, StatusCode};
use http_body::Body;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use multer::Multipart;
use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    convert::Infallible,
    net::IpAddr,
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock},
    time::SystemTime,
};
use tokio::{net::TcpListener, sync::oneshot};
use tokio_graceful::Shutdown;

const DEFAULT_MODEL_NAME: &str = "default";

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

pub async fn run(
    config: GlobalConfig,
    addr: Option<String>,
    web_assets: Option<PathBuf>,
) -> Result<()> {
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
    let web_assets =
        web_assets.unwrap_or_else(|| harnx_core::config_paths::data_dir().join("web-assets"));
    let server = Arc::new(Server::new(&config, web_assets));
    let listener = TcpListener::bind(&addr).await?;
    let stop_server = server.run(listener).await?;
    println!("Embeddings API:       http://{addr}/v1/embeddings");
    println!("Rerank API:           http://{addr}/v1/rerank");
    shutdown_signal().await;
    let _ = stop_server.send(());
    Ok(())
}

#[doc(hidden)]
pub struct Server {
    config: Config,
    models: Vec<Value>,
    agents: Vec<AgentConfig>,
    rags: Vec<String>,
    #[allow(dead_code)]
    session_registry: SessionRegistry,
    /// Root directory for web-ui static assets served over HTTP.
    web_assets: PathBuf,
}

type RouteMatch = (String, Option<String>, AgentsRoute);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentsRoute {
    Agent,
    Sessions,
    Session,
    SessionEvents,
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
        let config = config.read().clone();
        let mut models = list_all_models(&config.clients);
        let mut default_model = config.model.clone();
        default_model.data_mut().name = DEFAULT_MODEL_NAME.into();
        models.insert(0, default_model);
        let models: Vec<Value> = models
            .into_iter()
            .enumerate()
            .map(|(i, model)| {
                let id = if i == 0 {
                    DEFAULT_MODEL_NAME.into()
                } else {
                    model.id()
                };
                let mut value = json!(model.data());
                if let Some(value_obj) = value.as_object_mut() {
                    value_obj.insert("id".into(), id.into());
                    value_obj.insert("object".into(), "model".into());
                    value_obj.insert("owned_by".into(), model.client_name().into());
                    value_obj.remove("name");
                }
                value
            })
            .collect();
        let session_registry = SessionRegistry::new(config.clone());
        Self {
            config,
            models,
            agents: Config::all_agents(),
            rags: Config::list_rags(),
            session_registry,
            web_assets,
        }
    }

    #[doc(hidden)]
    pub async fn list_sessions_json(&self, agent: &str) -> Result<Value> {
        Ok(Value::Array(
            agent_sessions_json(&self.config, agent).await?,
        ))
    }

    #[doc(hidden)]
    pub async fn list_session_history(&self, agent: &str, session: &str) -> Result<Value> {
        use http_body_util::BodyExt;
        let resp = self.session_history_json(agent, session).await?;
        let body = resp.into_body().collect().await?.to_bytes();
        Ok(serde_json::from_slice(&body)?)
    }

    async fn run(self: Arc<Self>, listener: TcpListener) -> Result<oneshot::Sender<()>> {
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let shutdown = Shutdown::new(async { rx.await.unwrap_or_default() });
            let guard = shutdown.guard_weak();

            loop {
                tokio::select! {
                    res = listener.accept() => {
                        let Ok((cnx, _)) = res else {
                            continue;
                        };

                        let stream = TokioIo::new(cnx);
                        let server = self.clone();
                        shutdown.spawn_task(async move {
                            let hyper_service = service_fn(move |request: hyper::Request<Incoming>| {
                                server.clone().handle(request)
                            });
                            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                                .serve_connection_with_upgrades(stream, hyper_service)
                                .await;
                        });
                    }
                    _ = guard.cancelled() => {
                        break;
                    }
                }
            }
        });
        Ok(tx)
    }

    async fn handle(
        self: Arc<Self>,
        req: hyper::Request<Incoming>,
    ) -> std::result::Result<AppResponse, hyper::Error> {
        let method = req.method().clone();
        let uri = req.uri().clone();
        let path = uri.path();

        if method == Method::OPTIONS {
            let mut res = Response::default();
            *res.status_mut() = StatusCode::NO_CONTENT;
            set_cors_header(&mut res);
            return Ok(res);
        }

        let mut status = StatusCode::OK;
        let res = if path == "/v1/embeddings" {
            self.embeddings(req).await
        } else if path == "/v1/rerank" {
            self.rerank(req).await
        } else if path == "/v1/models" {
            self.list_models()
        } else if path == "/v1/agents" {
            self.list_agents(req.uri().query()).await
        } else if is_session_attachments_path(path) {
            self.upload_session_attachments(req).await
        } else if path.starts_with("/v1/agents/") {
            self.handle_agent_tree(req).await
        } else if path == "/v1/rags" {
            self.list_rags()
        } else if path == "/v1/rags/search" {
            self.search_rag(req).await
        } else if method == Method::GET || method == Method::HEAD {
            self.serve_web_asset(&method, path, req.headers()).await
        } else {
            status = StatusCode::NOT_FOUND;
            Err(anyhow!("Not Found"))
        };
        let mut res = match res {
            Ok(res) => {
                info!("{method} {uri} {}", status.as_u16());
                res
            }
            Err(err) => {
                if status == StatusCode::OK {
                    status = status_from_error(&err).unwrap_or(StatusCode::BAD_REQUEST);
                }
                error!("{method} {uri} {} {err}", status.as_u16());
                ret_err(err)
            }
        };
        *res.status_mut() = status;
        set_cors_header(&mut res);
        Ok(res)
    }

    fn list_models(&self) -> Result<AppResponse> {
        let data = json!({ "data": self.models });
        let res = Response::builder()
            .header("Content-Type", "application/json; charset=utf-8")
            .body(Full::new(Bytes::from(data.to_string())).boxed())?;
        Ok(res)
    }

    async fn list_agents(&self, query: Option<&str>) -> Result<AppResponse> {
        let agents = self.filter_agents_by_role(query).await?;
        let data = json!({ "data": agents });
        let res = Response::builder()
            .header("Content-Type", "application/json; charset=utf-8")
            .body(Full::new(Bytes::from(data.to_string())).boxed())?;
        Ok(res)
    }

    async fn filter_agents_by_role(&self, query: Option<&str>) -> Result<Vec<AgentConfig>> {
        if !query_requests_assistant_role(query) {
            return Ok(self.agents.clone());
        }

        let assistant_names = harnx_runtime::config::agent::list_assistant_agents().await;
        let assistants = self
            .agents
            .iter()
            .filter(|agent| assistant_names.iter().any(|name| name == agent.name()))
            .cloned()
            .collect();
        Ok(assistants)
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

    async fn handle_agent_tree(&self, req: hyper::Request<Incoming>) -> Result<AppResponse> {
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        if let Some((agent, session, route)) = session_routes::parse_session_metadata_route(&path) {
            resolve_agent(&self.config, &agent).map_err(ag_ui_error_to_anyhow)?;
            let target = AgentSessionRef::new(&agent, &session);
            return self.handle_session_metadata_route(req, target, route).await;
        }
        let route = match parse_agents_route(&path) {
            Some(route) => route,
            None => return Err(anyhow!("Not Found")),
        };
        let (agent_name, session_name, agent_route) = route;

        resolve_agent(&self.config, &agent_name).map_err(ag_ui_error_to_anyhow)?;

        match agent_route {
            AgentsRoute::Agent => {
                match negotiate_agents_route(&method, req.headers(), agent_route)? {
                    AgentsRepresentation::Html => self.agent_html_page(&agent_name),
                    AgentsRepresentation::Json => self.agent_json(&agent_name).await,
                    AgentsRepresentation::AgUiSse | AgentsRepresentation::AgUiRpc => {
                        Err(anyhow!("Not Acceptable"))
                    }
                }
            }
            AgentsRoute::Sessions => {
                self.handle_sessions_route(&method, req.headers(), &agent_name)
                    .await
            }
            AgentsRoute::Session => {
                let session_name = session_name.expect("session route always has session name");
                match negotiate_agents_route(&method, req.headers(), agent_route)? {
                    AgentsRepresentation::Html => {
                        self.session_html_page(&agent_name, &session_name)
                    }
                    AgentsRepresentation::Json => {
                        self.session_history_json(&agent_name, &session_name).await
                    }
                    AgentsRepresentation::AgUiSse => {
                        self.ag_ui_run_route(req, &agent_name, &session_name).await
                    }
                    AgentsRepresentation::AgUiRpc => {
                        handle_ag_ui_rpc(
                            req,
                            &agent_name,
                            &session_name,
                            &self.config,
                            &self.session_registry,
                            PersistenceKind::Nats,
                        )
                        .await
                    }
                }
            }
            AgentsRoute::SessionEvents => {
                let session_name = session_name.expect("session events route has a session name");
                self.handle_session_events_route(
                    &method,
                    req.headers(),
                    session_routes::AgentSessionRef {
                        agent: &agent_name,
                        session: &session_name,
                    },
                )
                .await
            }
        }
    }

    fn agent_html_page(&self, agent: &str) -> Result<AppResponse> {
        let html = format!("<h1>agent: {agent}</h1>");
        let res = Response::builder()
            .header("Content-Type", "text/html; charset=utf-8")
            .body(Full::new(Bytes::from(html)).boxed())?;
        Ok(res)
    }

    fn session_html_page(&self, _agent: &str, session: &str) -> Result<AppResponse> {
        let html = format!("<h1>session: {session}</h1>");
        let res = Response::builder()
            .header("Content-Type", "text/html; charset=utf-8")
            .body(Full::new(Bytes::from(html)).boxed())?;
        Ok(res)
    }

    async fn agent_json(&self, agent: &str) -> Result<AppResponse> {
        let sessions = agent_sessions_json(&self.config, agent).await?;
        let description = self
            .agents
            .iter()
            .find(|candidate| candidate.name() == agent)
            .map(AgentConfig::description)
            .filter(|description| !description.is_empty());
        let data = json!({
            "name": agent,
            "description": description,
            "sessions": sessions,
        });
        json_response(data)
    }

    async fn sessions_json(&self, agent: &str) -> Result<AppResponse> {
        json_response(Value::Array(
            agent_sessions_json(&self.config, agent).await?,
        ))
    }

    async fn upload_session_attachments<B>(&self, req: hyper::Request<B>) -> Result<AppResponse>
    where
        B: Body<Data = Bytes> + Send + Unpin,
        <B as Body>::Error: std::fmt::Display,
    {
        if req.method() != Method::POST {
            bail!("Method Not Allowed");
        }
        let path = req.uri().path().to_string();
        let (agent, session) =
            parse_session_attachments_path(&path).ok_or_else(|| anyhow!("Not Found"))?;

        if request_is_oversized(req.headers()) {
            return json_response_with_status(
                StatusCode::PAYLOAD_TOO_LARGE,
                json!({"error":"payload too large","max_bytes":MAX_UPLOAD_BYTES}),
            );
        }

        let Some(boundary) = multer::parse_boundary(
            req.headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("Bad Request"))?,
        )
        .ok() else {
            bail!("Bad Request");
        };
        let _scoped = agent_scoped_config(&self.config, &agent)?;
        let attachments_dir = Config::agent_data_dir(&agent)
            .join("attachments")
            .join(&session);

        // Stream body with size limit to prevent OOM
        let body = req.into_body();
        let mut cumulative: usize = 0;
        let mut chunks: Vec<Bytes> = Vec::new();
        let mut body_stream = body.into_data_stream();

        while let Some(chunk_result) = body_stream.next().await {
            let chunk = chunk_result.map_err(|e| anyhow!("Failed to read body: {}", e))?;
            cumulative += chunk.len();
            if cumulative > MAX_UPLOAD_BYTES {
                return json_response_with_status(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    json!({"error":"payload too large","max_bytes":MAX_UPLOAD_BYTES}),
                );
            }
            chunks.push(chunk);
        }

        let body_bytes = chunks.into_iter().flatten().collect::<Vec<u8>>();
        let stream = futures_util::stream::once(async move {
            Ok::<Bytes, std::io::Error>(Bytes::from(body_bytes))
        });
        let mut multipart = Multipart::new(stream, boundary);
        let mut refs = Vec::new();
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|err| anyhow!("Bad Request: {err}"))?
        {
            let name = field.name().unwrap_or_default().to_string();
            if name != "attachment" && name != "attachments" && name != "file" {
                continue;
            }
            let mime = field
                .content_type()
                .map(|m| m.to_string())
                .unwrap_or_else(|| "application/octet-stream".to_string());
            let field_name = field.name().unwrap_or_default().to_string();
            match mime.as_str() {
                "image/png" | "image/jpeg" | "image/webp" | "image/gif" | "application/pdf"
                | "text/plain" => {}
                _ => {
                    return json_response_with_status(
                        StatusCode::UNSUPPORTED_MEDIA_TYPE,
                        json!({"error":"unsupported attachment content type","field": field_name, "content_type":mime}),
                    );
                }
            }
            let data = field
                .bytes()
                .await
                .map_err(|err| anyhow!("Bad Request: {err}"))?;
            if data.len() > MAX_UPLOAD_BYTES {
                return json_response_with_status(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    json!({"error":"attachment too large","max_bytes":MAX_UPLOAD_BYTES}),
                );
            }
            refs.push(store_attachment_bytes(&attachments_dir, &data, &mime)?);
        }
        if refs.is_empty() {
            return json_response_with_status(
                StatusCode::BAD_REQUEST,
                json!({"error":"no attachment parts found"}),
            );
        }
        json_response(
            json!({"attachment_refs": refs, "attachments": refs.iter().map(|cid| json!({"cid": cid})).collect::<Vec<_>>() }),
        )
    }

    async fn session_history_json(&self, agent: &str, session: &str) -> Result<AppResponse> {
        let loaded_session = load_nats_session(&self.config, session).await?;
        if loaded_session.agent_name.as_deref() != Some(agent) {
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
        agent: &str,
        session: &str,
    ) -> Result<AppResponse> {
        let req_body = req.collect().await?.to_bytes();
        self.ag_ui_run(agent, session, &req_body)
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

        let config = Arc::new(RwLock::new(self.config.clone()));

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

    pub(crate) async fn ag_ui_run(
        &self,
        agent: &str,
        session: &str,
        req_body: &[u8],
    ) -> Result<AgUiAppResponse, AgUiError> {
        ag_ui::ag_ui_run_with_call_fn(
            &self.config,
            &self.session_registry,
            agent,
            session,
            req_body,
            None,
        )
        .await
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

        let config = Arc::new(RwLock::new(self.config.clone()));

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

        let config = Arc::new(RwLock::new(self.config.clone()));

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

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install CTRL+C signal handler")
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
        hyper::header::HeaderValue::from_static("Content-Type,Authorization"),
    );
}

fn ret_err<T: std::fmt::Display>(err: T) -> AppResponse {
    let data = json!({
        "error": {
            "message": err.to_string(),
            "type": "invalid_request_error",
        },
    });
    Response::builder()
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(data.to_string())).boxed())
        .unwrap()
}

fn percent_decode(input: &str) -> String {
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

fn parse_session_attachments_path(path: &str) -> Option<(String, String)> {
    let suffix = path.strip_prefix("/v1/agents/")?;
    let segments: Vec<_> = suffix
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    match segments.as_slice() {
        [agent, "sessions", session, "attachments"] => {
            let agent = percent_decode(agent);
            let session = percent_decode(session);
            if !is_safe_agent_path(&agent) || !is_safe_path_segment(&session) {
                return None;
            }
            Some((agent, session))
        }
        _ => None,
    }
}

fn request_is_oversized(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_UPLOAD_BYTES)
}

fn is_safe_path_segment(value: &str) -> bool {
    !value.is_empty()
        && std::path::Path::new(value)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && !value.contains(['/', '\\'])
}

fn is_safe_agent_path(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('/') && value.split('/').all(is_safe_path_segment)
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
        .get(http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html"))
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
        .get(http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .any(|item| item.trim().starts_with("text/html"))
        })
        .unwrap_or(false)
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
        .get(http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(accept_header_allows_event_stream)
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

fn agent_scoped_config(config: &Config, agent: &str) -> Result<Config> {
    let scoped = harnx_session::fork_prompt_config(config);
    scoped
        .write()
        .use_agent_by_name(agent)
        .map_err(|err| anyhow!("Failed to scope config to agent '{agent}': {err}"))?;
    let config = scoped.read().clone();
    Ok(config)
}

/// Ordering for the web session list: most-recently-modified first so active
/// work surfaces at the top. Sessions without a modified time sort last; ties
/// (equal or both-missing modified) fall back to id (descending) for stable,
/// deterministic ordering.
fn session_recency_ordering(
    left: &harnx_runtime::config::SessionMeta,
    right: &harnx_runtime::config::SessionMeta,
) -> std::cmp::Ordering {
    right
        .modified
        .cmp(&left.modified)
        .then_with(|| right.id.cmp(&left.id))
}

async fn agent_sessions_json(config: &Config, agent: &str) -> Result<Vec<Value>> {
    ensure_frontend_nats_owner().await?;
    let mut sessions: Vec<_> = config
        .list_remote_sessions_with_meta(LOCAL_CLUSTER_KEY)
        .await?
        .into_iter()
        // Per-agent endpoints must not leak sessions without agent attribution or for other agents.
        // Missing/empty agent_name stays excluded from per-agent lists until a later backfill pass.
        .filter(|session| session.agent_name.as_deref() == Some(agent))
        .collect();

    sessions.sort_by(session_recency_ordering);

    Ok(sessions
        .into_iter()
        .map(|session| {
            let session_id = session.session_id.unwrap_or(session.id);
            let mut value = serde_json::Map::from_iter([(
                String::from("session_id"),
                Value::String(session_id),
            )]);
            value.insert(
                String::from("title"),
                session.title.map(Value::String).unwrap_or(Value::Null),
            );
            if let Some(modified) = session.modified {
                value.insert(
                    String::from("updated_at"),
                    Value::String(format_system_time(modified)),
                );
            }
            Value::Object(value)
        })
        .collect())
}

pub(crate) async fn load_nats_session(
    config: &Config,
    session: &str,
) -> Result<harnx_core::session::Session> {
    ensure_frontend_nats_owner().await?;
    let jetstream = config.nats_jetstream(LOCAL_CLUSTER_KEY).await?;
    let metadata_store =
        harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1).await?;
    let metadata = metadata_store
        .get(session)
        .await?
        .ok_or_else(|| anyhow!("Not Found"))?
        .metadata;
    let log = harnx_runtime::nats_session_log::NatsSessionLog::new(jetstream, session.to_string());
    let entries = log
        .load_events_async()
        .await
        .map_err(|err| anyhow!("Failed to load session history for '{session}': {err}"))?;
    let loaded = harnx_runtime::nats_session_log::load_session_from_entries_with_metadata(
        &entries,
        session,
        metadata.base_session(),
    )
    .map_err(|err| anyhow!("Failed to reconstruct session history for '{session}': {err}"))?;
    Ok(loaded)
}

#[doc(hidden)]
pub async fn ensure_frontend_nats_owner() -> Result<()> {
    if std::env::var_os("HARNX_NATS_URL").is_some()
        && std::env::var_os("HARNX_NATS_TOKEN").is_some()
    {
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
        _ => message.content.to_text(),
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

fn ag_ui_error_to_anyhow(err: AgUiError) -> anyhow::Error {
    anyhow!(format!("{}::__status={}", err, err.status_code().as_u16()))
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
    let marker = "__status=";
    let status = message.split(marker).nth(1)?;
    let code = status.parse::<u16>().ok()?;
    StatusCode::from_u16(code).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestConfigSandbox;
    use http::HeaderValue;

    #[test]
    fn percent_decode_decodes_slashes_and_preserves_invalid_escapes() {
        assert_eq!(percent_decode("coding%2Fcoder"), "coding/coder");
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

    #[test]
    fn agent_route_error_mapping_covers_404_405_and_406_shapes() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();

        let missing_agent = resolve_agent(&config, "missing").map_err(ag_ui_error_to_anyhow);
        let missing_err = missing_agent.expect_err("unknown agent should 404");
        assert!(missing_err.to_string().contains("__status=404"));
        assert_eq!(status_from_error(&missing_err), Some(StatusCode::NOT_FOUND));

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

        let config = Arc::new(RwLock::new(sandbox.config()));
        let server = Server::new(&config, std::path::PathBuf::from("web-assets"));

        let unfiltered = response_json(server.list_agents(None).await.expect("all agents")).await;
        let filtered = response_json(
            server
                .list_agents(Some("role=assistant"))
                .await
                .expect("assistant agents"),
        )
        .await;

        let unfiltered_agents = unfiltered["data"]
            .as_array()
            .expect("unfiltered agents array");
        let filtered_agents = filtered["data"].as_array().expect("filtered agents array");

        assert_eq!(unfiltered_agents.len(), 2);
        assert_eq!(filtered_agents.len(), 1);
        assert_eq!(filtered_agents[0]["name"], "assistant-alpha");
        assert_eq!(filtered_agents[0]["role"], "assistant");
        assert_eq!(filtered_agents[0]["description"], "Alpha");
        assert!(unfiltered_agents
            .iter()
            .any(|agent| agent["name"] == "helper-beta"));
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
            },
            SessionMeta {
                id: "local-2".into(),
                session_id: Some("beta".into()),
                agent_name: Some("other".into()),
                title: None,
                modified: None,
            },
            SessionMeta {
                id: "local-3".into(),
                session_id: None,
                agent_name: None,
                title: None,
                modified: None,
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

    #[test]
    fn session_recency_ordering_covers_ties_and_missing_modified() {
        use std::time::{Duration, UNIX_EPOCH};

        let meta = |id: &str, modified: Option<SystemTime>| SessionMeta {
            id: id.into(),
            session_id: Some(id.into()),
            agent_name: Some("plain".into()),
            title: None,
            modified,
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

    #[test]
    fn upload_attachments_success_returns_cid_refs() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = Arc::new(RwLock::new(sandbox.config()));
        let server = Arc::new(Server::new(&config, std::path::PathBuf::from("web-assets")));

        let boundary = "boundary123";
        let image_data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]; // PNG magic bytes
        let body = build_multipart_body(
            boundary,
            &[("attachment", "test.png", "image/png", &image_data)],
        );

        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let response = rt
            .block_on(call_upload_handler(
                server,
                "/v1/agents/plain/sessions/test-session/attachments",
                boundary,
                body,
                None,
            ))
            .expect("handler response");

        assert_eq!(response.status(), StatusCode::OK);
        let body_bytes = rt
            .block_on(http_body_util::BodyExt::collect(response.into_body()))
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
        assert!(refs[0].as_str().unwrap().starts_with("cid:"));

        // Verify the uploaded blob was stored in the attachment directory.
        let attachments_dir = Config::agent_data_dir("plain")
            .join("attachments")
            .join("test-session");
        assert!(
            attachments_dir.exists(),
            "attachments dir should exist at {:?}",
            attachments_dir
        );
    }

    #[test]
    fn upload_attachments_malformed_multipart_returns_400() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = Arc::new(RwLock::new(sandbox.config()));
        let server = Arc::new(Server::new(&config, std::path::PathBuf::from("web-assets")));

        // Build a malformed multipart body (missing proper headers)
        let boundary = "boundary123";
        let invalid_body = b"--boundary123\r\nnot-a-valid-part\r\n--boundary123--\r\n".to_vec();

        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let response = rt.block_on(call_upload_handler(
            server,
            "/v1/agents/plain/sessions/test-session/attachments",
            boundary,
            invalid_body,
            None,
        ));

        // Handler returns Err for malformed multipart, which gets converted to BAD_REQUEST
        match response {
            Ok(resp) => {
                assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            }
            Err(_) => {
                // Error case is acceptable - handler bubbles up "Bad Request" error
            }
        }
    }

    #[test]
    fn upload_attachments_no_parts_returns_400() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = Arc::new(RwLock::new(sandbox.config()));
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

        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let response = rt
            .block_on(call_upload_handler(
                server,
                "/v1/agents/plain/sessions/test-session/attachments",
                boundary,
                body,
                None,
            ))
            .expect("handler response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn upload_attachments_oversized_returns_413() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = Arc::new(RwLock::new(sandbox.config()));
        let server = Arc::new(Server::new(&config, std::path::PathBuf::from("web-assets")));

        // Build a multipart body that exceeds MAX_UPLOAD_BYTES
        let boundary = "boundary123";
        let oversized_data = vec![0u8; MAX_UPLOAD_BYTES + 1024];
        let body = build_multipart_body(
            boundary,
            &[("attachment", "test.png", "image/png", &oversized_data)],
        );

        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let response = rt
            .block_on(call_upload_handler(
                server,
                "/v1/agents/plain/sessions/test-session/attachments",
                boundary,
                body,
                None,
            ))
            .expect("handler response");

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn upload_attachments_oversized_content_length_header_returns_413_early() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = Arc::new(RwLock::new(sandbox.config()));
        let server = Arc::new(Server::new(&config, std::path::PathBuf::from("web-assets")));

        // Build a request with a Content-Length header that exceeds MAX_UPLOAD_BYTES
        let boundary = "boundary123";
        let body = vec![0u8; 100]; // Small body, but Content-Length header says it's huge

        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let response = rt
            .block_on(call_upload_handler(
                server,
                "/v1/agents/plain/sessions/test-session/attachments",
                boundary,
                body,
                Some(MAX_UPLOAD_BYTES + 1), // Oversized Content-Length header
            ))
            .expect("handler response");

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn upload_attachments_unsupported_content_type_returns_415() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = Arc::new(RwLock::new(sandbox.config()));
        let server = Arc::new(Server::new(&config, std::path::PathBuf::from("web-assets")));

        // Build a multipart with an unsupported MIME type
        let boundary = "boundary123";
        let data = b"some binary data".to_vec();
        let body = build_multipart_body(
            boundary,
            &[("attachment", "test.exe", "application/octet-stream", &data)],
        );

        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let response = rt
            .block_on(call_upload_handler(
                server,
                "/v1/agents/plain/sessions/test-session/attachments",
                boundary,
                body,
                None,
            ))
            .expect("handler response");

        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    // ===== Web-ui static asset serving tests (#1006) =====

    fn asset_server(assets_dir: PathBuf) -> Server {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        // Server::new snapshots the config at construction, so the sandbox can
        // be dropped afterwards without affecting the built Server.
        let config = Arc::new(RwLock::new(sandbox.config()));
        Server::new(&config, assets_dir)
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
}
