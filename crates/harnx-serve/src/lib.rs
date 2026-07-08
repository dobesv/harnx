//! harnx-serve — HTTP server front-end for the harnx agent harness
//! (plan P47, β+ progressive peel). Extracted from `harnx::serve`.
//! Depends on `harnx-runtime` for Config + Client orchestration.

pub mod ag_ui;
pub mod ag_ui_rpc;
pub mod session_actor;
// Not `#[cfg(test)]`: the `tests/` integration crates link the library built
// WITHOUT the `test` cfg, so gating this out would break their
// `harnx_serve::test_support` imports. Kept public for cross-crate test reuse.
pub mod test_support;

use crate::ag_ui::{resolve_agent, AgUiError, AppResponse as AgUiAppResponse};
use crate::ag_ui_rpc::{handle_ag_ui_rpc, PersistenceKind};
use crate::session_actor::SessionRegistry;

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
use std::{collections::BTreeMap, convert::Infallible, net::IpAddr, sync::Arc, time::SystemTime};
use tokio::{net::TcpListener, sync::oneshot};
use tokio_graceful::Shutdown;

const DEFAULT_MODEL_NAME: &str = "default";

/// Maximum upload size in bytes (20 MiB).
/// Enforced during streaming to prevent OOM from oversized payloads.
pub const MAX_UPLOAD_BYTES: usize = 20 * 1024 * 1024;

type AppResponse = Response<BoxBody<Bytes, Infallible>>;

pub async fn run(config: GlobalConfig, addr: Option<String>) -> Result<()> {
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
    let server = Arc::new(Server::new(&config));
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
}

type RouteMatch = (String, Option<String>, AgentsRoute);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentsRoute {
    Agent,
    Sessions,
    Session,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentsRepresentation {
    Html,
    Json,
    AgUiSse,
}

impl Server {
    #[doc(hidden)]
    pub fn new(config: &GlobalConfig) -> Self {
        let config = config.read().clone();
        let mut models = list_all_models(&config.clients);
        let mut default_model = config.model.clone();
        default_model.data_mut().name = DEFAULT_MODEL_NAME.into();
        models.insert(0, &default_model);
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
        }
    }

    #[doc(hidden)]
    pub fn list_sessions_json(&self, agent: &str) -> Result<Value> {
        Ok(Value::Array(agent_sessions_json(&self.config, agent)?))
    }

    #[doc(hidden)]
    pub async fn list_session_history(&self, agent: &str, session: &str) -> Result<Value> {
        use http_body_util::BodyExt;
        let resp = self.session_history_json(agent, session)?;
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
        } else if is_agent_rpc_path(path) {
            let persistence = if self.config.nats_servers.is_empty() {
                PersistenceKind::Filesystem
            } else {
                PersistenceKind::Nats
            };
            handle_ag_ui_rpc(req, &self.config, &self.session_registry, persistence).await
        } else if is_session_attachments_path(path) {
            self.upload_session_attachments(req).await
        } else if path.starts_with("/v1/agents/") {
            self.handle_agent_tree(req).await
        } else if path == "/v1/rags" {
            self.list_rags()
        } else if path == "/v1/rags/search" {
            self.search_rag(req).await
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

    async fn handle_agent_tree(&self, req: hyper::Request<Incoming>) -> Result<AppResponse> {
        let method = req.method().clone();
        let path = req.uri().path().to_string();
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
                    AgentsRepresentation::Json => self.agent_json(&agent_name),
                    AgentsRepresentation::AgUiSse => Err(anyhow!("Not Acceptable")),
                }
            }
            AgentsRoute::Sessions => {
                match negotiate_agents_route(&method, req.headers(), agent_route)? {
                    AgentsRepresentation::Json => self.sessions_json(&agent_name),
                    AgentsRepresentation::Html | AgentsRepresentation::AgUiSse => {
                        Err(anyhow!("Not Acceptable"))
                    }
                }
            }
            AgentsRoute::Session => {
                let session_name = session_name.expect("session route always has session name");
                match negotiate_agents_route(&method, req.headers(), agent_route)? {
                    AgentsRepresentation::Html => {
                        self.session_html_page(&agent_name, &session_name)
                    }
                    AgentsRepresentation::Json => {
                        self.session_history_json(&agent_name, &session_name)
                    }
                    AgentsRepresentation::AgUiSse => {
                        self.ag_ui_run_route(req, &agent_name, &session_name).await
                    }
                }
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

    fn agent_json(&self, agent: &str) -> Result<AppResponse> {
        let sessions = agent_sessions_json(&self.config, agent)?;
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

    fn sessions_json(&self, agent: &str) -> Result<AppResponse> {
        json_response(Value::Array(agent_sessions_json(&self.config, agent)?))
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

        // Check Content-Length header first (early rejection for oversized payloads)
        if let Some(length) = req
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
        {
            if length > MAX_UPLOAD_BYTES {
                return json_response_with_status(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    json!({"error":"payload too large","max_bytes":MAX_UPLOAD_BYTES}),
                );
            }
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
        let scoped = agent_scoped_config(&self.config, &agent)?;
        let session_path = scoped.session_file(&session);
        let attachments_dir = session_path.with_extension("attachments");

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

    fn session_history_json(&self, agent: &str, session: &str) -> Result<AppResponse> {
        let config = agent_scoped_config(&self.config, agent)?;
        let session_path = config.session_file(session);
        if !session_path.exists() {
            bail!("Not Found");
        }

        let loaded_session = harnx_runtime::config::session::load(&config, session, &session_path)
            .map_err(|err| anyhow!("Failed to load session history for '{session}': {err}"))?;
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

/// Matches exactly the JSON-RPC control-plane path shape `/v1/agents/{agent}/rpc`
/// (a single agent segment followed by `rpc`). A naive `ends_with("/rpc")` check
/// would also match a session literally named `rpc`
/// (`/v1/agents/{agent}/sessions/rpc`) and misroute it to the RPC handler.
fn is_agent_rpc_path(path: &str) -> bool {
    let Some(suffix) = path.strip_prefix("/v1/agents/") else {
        return false;
    };
    let segments: Vec<_> = suffix
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    matches!(segments.as_slice(), [_agent, "rpc"])
        || matches!(segments.as_slice(), [_agent, "sessions", _, "rpc"])
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
            Some((percent_decode(agent), percent_decode(session)))
        }
        _ => None,
    }
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
        (&Method::GET, AgentsRoute::Sessions) => Ok(AgentsRepresentation::Json),
        (&Method::POST, AgentsRoute::Session) => {
            if accepts_event_stream(headers) || content_type_is_json(headers) {
                Ok(AgentsRepresentation::AgUiSse)
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

fn accepts_event_stream(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"))
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

fn agent_sessions_json(config: &Config, agent: &str) -> Result<Vec<Value>> {
    Ok(agent_scoped_config(config, agent)?
        .list_sessions_with_meta()
        .into_iter()
        // Per-agent endpoints must not leak sessions without agent attribution or for other agents.
        // Missing/empty agent_name stays excluded from per-agent lists until a later backfill pass.
        .filter(|session| session.agent_name.as_deref() == Some(agent))
        .map(|session| {
            let session_id = session.session_id.unwrap_or(session.id);
            let mut value = serde_json::Map::from_iter([(
                String::from("session_id"),
                Value::String(session_id),
            )]);
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

fn format_system_time(value: SystemTime) -> String {
    DateTime::<Utc>::from(value).to_rfc3339()
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
        .is_some_and(|value| value.starts_with("application/json"))
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
        assert_eq!(parse_agents_route("/v1/agents"), None);
        assert_eq!(parse_agents_route("/v1/agents/hephaestus/extra"), None);
        assert_eq!(
            parse_agents_route("/v1/agents/hephaestus%2Fsessions"),
            Some(("hephaestus/sessions".to_string(), None, AgentsRoute::Agent,))
        );
    }

    #[test]
    fn is_agent_rpc_path_matches_agent_and_session_rpc_shapes_only() {
        assert!(is_agent_rpc_path("/v1/agents/hephaestus/rpc"));
        assert!(is_agent_rpc_path(
            "/v1/agents/hephaestus/sessions/thread-1/rpc"
        ));
        assert!(is_agent_rpc_path(
            "/v1/agents/coding%2Fcoder/sessions/thread-1/rpc"
        ));
        // A session literally named "rpc" without trailing RPC segment must NOT be treated as RPC.
        assert!(!is_agent_rpc_path("/v1/agents/hephaestus/sessions/rpc"));
        // Other agent-tree shapes are not RPC.
        assert!(!is_agent_rpc_path("/v1/agents/hephaestus"));
        assert!(!is_agent_rpc_path("/v1/agents/hephaestus/sessions"));
        assert!(!is_agent_rpc_path(
            "/v1/agents/hephaestus/sessions/thread-1"
        ));
        assert!(!is_agent_rpc_path(
            "/v1/agents/hephaestus/sessions/thread-1/attachments"
        ));
        assert!(!is_agent_rpc_path("/v1/models"));
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
        assert!(!is_session_attachments_path("/v1/agents/hephaestus/rpc"));
        assert!(!is_session_attachments_path(
            "/v1/agents/hephaestus/sessions/thread-1"
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
    fn negotiate_agents_route_accept_header_drives_html_json_and_sse_cases() {
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
        let server = Server::new(&config);

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
                working_dir: None,
                git_branch: None,
                git_remote: None,
                terminal_session_id: None,
                agent_name: Some("plain".into()),
                modified: None,
            },
            SessionMeta {
                id: "local-2".into(),
                session_id: Some("beta".into()),
                working_dir: None,
                git_branch: None,
                git_remote: None,
                terminal_session_id: None,
                agent_name: Some("other".into()),
                modified: None,
            },
            SessionMeta {
                id: "local-3".into(),
                session_id: None,
                working_dir: None,
                git_branch: None,
                git_remote: None,
                terminal_session_id: None,
                agent_name: None,
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
    fn agent_scoped_resolution_lists_and_loads_agent_sessions() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        sandbox.write_agent("other", "You are other.");
        let config = sandbox.config();

        let scoped = agent_scoped_config(&config, "plain").expect("scope config");
        let agent_session_path = scoped.session_file("akjG7w");
        std::fs::create_dir_all(
            agent_session_path
                .parent()
                .expect("agent session parent directory"),
        )
        .expect("create agent sessions dir");
        let flat_session_path = config.session_file("flat-only");
        std::fs::create_dir_all(
            flat_session_path
                .parent()
                .expect("flat session parent directory"),
        )
        .expect("create flat sessions dir");

        let prompt_config = harnx_session::fork_prompt_config(&config);
        {
            let mut prompt = prompt_config.write();
            prompt.use_agent_by_name("plain").expect("set agent");
            prompt.use_session(Some("akjG7w")).expect("open session");
            let session = prompt.session.as_mut().expect("session loaded");
            session.messages.push(Message::new(
                MessageRole::User,
                harnx_core::message::MessageContent::Text("hi from scoped dir".into()),
            ));
            harnx_runtime::config::session::save(session, "agent-real", &agent_session_path, false)
                .expect("save scoped session");
        }

        std::fs::write(
            &flat_session_path,
            concat!(
                "type: header
",
                "session_id: flat-only
",
                "working_dir: /tmp/project
",
                "agent_name: plain
",
                "---
",
                "type: message
",
                "role: user
",
                "content: hi from flat dir
",
            ),
        )
        .expect("write flat session");

        let listed = agent_sessions_json(&config, "plain").expect("list scoped sessions");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].get("session_id"), Some(&json!("akjG7w")));
        assert!(listed[0].get("updated_at").is_some());

        let loaded =
            harnx_runtime::config::session::load(&scoped, "agent-real", &agent_session_path)
                .expect("load scoped session");
        assert_eq!(loaded.agent_name.as_deref(), Some("plain"));
        assert!(!loaded.messages.is_empty());
        assert_eq!(
            history_message_content(loaded.messages.last().expect("latest message")),
            "hi from scoped dir"
        );
        assert!(!scoped.session_file("flat-only").exists());
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
    fn server_handle_routes_session_scoped_rpc_requests_to_rpc_handler() {
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

        assert!(is_agent_rpc_path(
            "/v1/agents/coding%2Fcoder/sessions/thread-1/rpc"
        ));
        assert!(!is_agent_rpc_path(
            "/v1/agents/coding%2Fcoder/sessions/thread-1"
        ));
        assert!(is_session_attachments_path(
            "/v1/agents/coding%2Fcoder/sessions/thread-1/attachments"
        ));
        let session_route = parse_agents_route("/v1/agents/coding%2Fcoder/sessions/thread-1")
            .expect("session route should parse");
        assert_eq!(session_route.0, "coding/coder");
        assert_eq!(session_route.1.as_deref(), Some("thread-1"));
        assert_eq!(session_route.2, AgentsRoute::Session);
        assert!(parse_agents_route("/v1/agents/coding%2Fcoder/sessions/thread-1/rpc").is_none());
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
        let server = Arc::new(Server::new(&config));

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

        // Verify file was stored in attachments directory
        // Note: The handler uses agent_scoped_config which scopes the config for the agent,
        // so we need to use the scoped config to find the session path
        let scoped = agent_scoped_config(&config.read(), "plain").expect("scope config");
        let session_path = scoped.session_file("test-session");
        let attachments_dir = session_path.with_extension("attachments");
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
        let server = Arc::new(Server::new(&config));

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
        let server = Arc::new(Server::new(&config));

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
        let server = Arc::new(Server::new(&config));

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
        let server = Arc::new(Server::new(&config));

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
        let server = Arc::new(Server::new(&config));

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
}
