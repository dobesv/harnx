//! harnx-serve — HTTP server front-end for the harnx agent harness
//! (plan P47, β+ progressive peel). Extracted from `harnx::serve`.
//! Depends on `harnx-runtime` for Config + Client orchestration.

mod ag_ui;

use crate::ag_ui::{fork_prompt_config, resolve_agent, AgUiError, AppResponse as AgUiAppResponse};

use harnx_core::message::{Message, MessageRole};
use harnx_rag::*;
use harnx_runtime::{client::*, config::*, tool::*, utils::*};
use log::{debug, error, info};

use anyhow::{anyhow, bail, Result};
use bytes::Bytes;
use chrono::{DateTime, Timelike, Utc};
use futures_util::StreamExt;
use http::{Method, Response, StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    service::service_fn,
};
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    net::IpAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::SystemTime,
};
use tokio::{
    net::TcpListener,
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
};
use tokio_graceful::Shutdown;
use tokio_stream::wrappers::UnboundedReceiverStream;

const DEFAULT_MODEL_NAME: &str = "default";
const PLAYGROUND_HTML: &[u8] = include_bytes!("../assets/playground.html");
const ARENA_HTML: &[u8] = include_bytes!("../assets/arena.html");

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
    println!("Chat Completions API: http://{addr}/v1/chat/completions");
    println!("Embeddings API:       http://{addr}/v1/embeddings");
    println!("Rerank API:           http://{addr}/v1/rerank");
    println!("LLM Playground:       http://{addr}/playground");
    println!("LLM Arena:            http://{addr}/arena?num=2");
    shutdown_signal().await;
    let _ = stop_server.send(());
    Ok(())
}

struct Server {
    config: Config,
    models: Vec<Value>,
    agents: Vec<AgentConfig>,
    rags: Vec<String>,
}

type RouteMatch<'a> = (&'a str, Option<&'a str>, AgentsRoute);

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
    fn new(config: &GlobalConfig) -> Self {
        let mut config = config.read().clone();
        config.tools = Tools::default();
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
        Self {
            config,
            models,
            agents: Config::all_agents(),
            rags: Config::list_rags(),
        }
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
        let res = if path == "/v1/chat/completions" {
            self.chat_completions(req).await
        } else if path == "/v1/embeddings" {
            self.embeddings(req).await
        } else if path == "/v1/rerank" {
            self.rerank(req).await
        } else if path == "/v1/models" {
            self.list_models()
        } else if path == "/v1/agents" {
            self.list_agents()
        } else if path.starts_with("/v1/agents/") {
            self.handle_agent_tree(req).await
        } else if path == "/v1/rags" {
            self.list_rags()
        } else if path == "/v1/rags/search" {
            self.search_rag(req).await
        } else if path == "/playground" || path == "/playground.html" {
            self.playground_page()
        } else if path == "/arena" || path == "/arena.html" {
            self.arena_page()
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

    fn playground_page(&self) -> Result<AppResponse> {
        let res = Response::builder()
            .header("Content-Type", "text/html; charset=utf-8")
            .body(Full::new(Bytes::from(PLAYGROUND_HTML)).boxed())?;
        Ok(res)
    }

    fn arena_page(&self) -> Result<AppResponse> {
        let res = Response::builder()
            .header("Content-Type", "text/html; charset=utf-8")
            .body(Full::new(Bytes::from(ARENA_HTML)).boxed())?;
        Ok(res)
    }

    fn list_models(&self) -> Result<AppResponse> {
        let data = json!({ "data": self.models });
        let res = Response::builder()
            .header("Content-Type", "application/json; charset=utf-8")
            .body(Full::new(Bytes::from(data.to_string())).boxed())?;
        Ok(res)
    }

    fn list_agents(&self) -> Result<AppResponse> {
        let data = json!({ "data": self.agents });
        let res = Response::builder()
            .header("Content-Type", "application/json; charset=utf-8")
            .body(Full::new(Bytes::from(data.to_string())).boxed())?;
        Ok(res)
    }

    async fn handle_agent_tree(&self, req: hyper::Request<Incoming>) -> Result<AppResponse> {
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        let route = match parse_agents_route(&path) {
            Some(route) => route,
            None => return Err(anyhow!("Not Found")),
        };
        let (agent_name, session_name, agent_route) = route;

        resolve_agent(&self.config, agent_name).map_err(ag_ui_error_to_anyhow)?;

        match agent_route {
            AgentsRoute::Agent => {
                match negotiate_agents_route(&method, req.headers(), agent_route)? {
                    AgentsRepresentation::Html => self.agent_html_page(agent_name),
                    AgentsRepresentation::Json => self.agent_json(agent_name),
                    AgentsRepresentation::AgUiSse => Err(anyhow!("Not Acceptable")),
                }
            }
            AgentsRoute::Sessions => {
                match negotiate_agents_route(&method, req.headers(), agent_route)? {
                    AgentsRepresentation::Json => self.sessions_json(agent_name),
                    AgentsRepresentation::Html | AgentsRepresentation::AgUiSse => {
                        Err(anyhow!("Not Acceptable"))
                    }
                }
            }
            AgentsRoute::Session => {
                let session_name = session_name.expect("session route always has session name");
                match negotiate_agents_route(&method, req.headers(), agent_route)? {
                    AgentsRepresentation::Html => self.session_html_page(agent_name, session_name),
                    AgentsRepresentation::Json => {
                        self.session_history_json(agent_name, session_name)
                    }
                    AgentsRepresentation::AgUiSse => {
                        self.ag_ui_run_route(req, agent_name, session_name).await
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

    async fn chat_completions(&self, req: hyper::Request<Incoming>) -> Result<AppResponse> {
        let req_body = req.collect().await?.to_bytes();
        let req_body: Value = serde_json::from_slice(&req_body)
            .map_err(|err| anyhow!("Invalid request json, {err}"))?;

        debug!("chat completions request: {req_body}");
        let req_body = serde_json::from_value(req_body)
            .map_err(|err| anyhow!("Invalid request body, {err}"))?;

        let ChatCompletionsReqBody {
            model,
            messages,
            temperature,
            top_p,
            max_tokens,
            stream,
            tools,
        } = req_body;

        let mut messages =
            parse_messages(messages).map_err(|err| anyhow!("Invalid request body, {err}"))?;

        let functions = parse_tools(tools).map_err(|err| anyhow!("Invalid request body, {err}"))?;

        let config = self.config.clone();

        let default_model = config.model.clone();

        let config = Arc::new(RwLock::new(config));

        let (model_name, change) = if model == DEFAULT_MODEL_NAME {
            (default_model.id(), true)
        } else if default_model.id() == model {
            (model, false)
        } else {
            (model, true)
        };

        if change {
            config.write().set_model(&model_name)?;
        }

        let mut client = {
            let guard = config.read();
            let model = guard.model.clone();
            init_client(&guard.clients, &model)?
        };
        if max_tokens.is_some() {
            client.model_mut().set_max_tokens(max_tokens, true);
        }
        let abort_signal = create_abort_signal();
        let (dry_run_flag, ua_owned) = {
            let cfg = config.read();
            (cfg.dry_run, cfg.user_agent.clone())
        };
        let call_ctx = harnx_runtime::client::ClientCallContext {
            user_agent: ua_owned.as_deref(),
            dry_run: dry_run_flag,
        };
        let http_client = client.build_client(&call_ctx)?;

        let completion_id = generate_completion_id();
        let created = Utc::now().timestamp();

        patch_messages(&mut messages, client.model());

        let data: ChatCompletionsData = ChatCompletionsData {
            messages,
            temperature,
            top_p,
            functions,
            stream,
            attachments_dir: None, // harnx-serve doesn't have session attachment dirs
        };

        if stream {
            let (tx, mut rx) = unbounded_channel();
            tokio::spawn(async move {
                let is_first = Arc::new(AtomicBool::new(true));
                let (sse_tx, sse_rx) = unbounded_channel();
                let mut handler = SseHandler::new(sse_tx, abort_signal);
                async fn map_event(
                    mut sse_rx: UnboundedReceiver<SseEvent>,
                    tx: &UnboundedSender<ResEvent>,
                    is_first: Arc<AtomicBool>,
                ) {
                    while let Some(reply_event) = sse_rx.recv().await {
                        if is_first.load(Ordering::SeqCst) {
                            let _ = tx.send(ResEvent::First(None));
                            is_first.store(false, Ordering::SeqCst)
                        }
                        match reply_event {
                            SseEvent::Text(text) => {
                                let _ = tx.send(ResEvent::Text(text));
                            }
                            SseEvent::Done => {
                                let _ = tx.send(ResEvent::Done);
                                sse_rx.close();
                            }
                        }
                    }
                }
                async fn chat_completions(
                    client: &dyn Client,
                    http_client: &reqwest::Client,
                    handler: &mut SseHandler,
                    mut data: ChatCompletionsData,
                    tx: &UnboundedSender<ResEvent>,
                    is_first: Arc<AtomicBool>,
                ) {
                    if client.model().no_stream() {
                        data.stream = false;
                        let ret = client.chat_completions_inner(http_client, data).await;
                        match ret {
                            Ok(output) => {
                                let ChatCompletionsOutput {
                                    text, tool_calls, ..
                                } = output;
                                let _ = tx.send(ResEvent::First(None));
                                is_first.store(false, Ordering::SeqCst);
                                let _ = tx.send(ResEvent::Text(text));
                                if !tool_calls.is_empty() {
                                    let _ = tx.send(ResEvent::ToolCalls(tool_calls));
                                }
                            }
                            Err(err) => {
                                let _ = tx.send(ResEvent::First(Some(format!("{err:?}"))));
                                is_first.store(false, Ordering::SeqCst)
                            }
                        };
                    } else {
                        let ret = client
                            .chat_completions_streaming_inner(http_client, handler, data)
                            .await;
                        let first = match ret {
                            Ok(()) => None,
                            Err(err) => Some(format!("{err:?}")),
                        };
                        if is_first.load(Ordering::SeqCst) {
                            let _ = tx.send(ResEvent::First(first));
                            is_first.store(false, Ordering::SeqCst)
                        }
                        let tool_calls = handler.tool_calls().to_vec();
                        if !tool_calls.is_empty() {
                            let _ = tx.send(ResEvent::ToolCalls(tool_calls));
                        }
                    }
                    handler.done();
                }
                tokio::join!(
                    map_event(sse_rx, &tx, is_first.clone()),
                    chat_completions(
                        client.as_ref(),
                        &http_client,
                        &mut handler,
                        data,
                        &tx,
                        is_first
                    ),
                );
            });

            let first_event = rx.recv().await;

            if let Some(ResEvent::First(Some(err))) = first_event {
                bail!("{err}");
            }

            let shared: Arc<(String, String, i64, AtomicBool)> =
                Arc::new((completion_id, model_name, created, AtomicBool::new(false)));
            let stream = UnboundedReceiverStream::new(rx);
            let stream = stream.filter_map(move |res_event| {
                let shared = shared.clone();
                async move {
                    let (completion_id, model, created, has_tool_calls) = shared.as_ref();
                    match res_event {
                        ResEvent::Text(text) => {
                            Some(Ok(create_text_frame(completion_id, model, *created, &text)))
                        }
                        ResEvent::ToolCalls(tool_calls) => {
                            has_tool_calls.store(true, Ordering::SeqCst);
                            Some(Ok(create_tool_calls_frame(
                                completion_id,
                                model,
                                *created,
                                &tool_calls,
                            )))
                        }
                        ResEvent::Done => Some(Ok(create_done_frame(
                            completion_id,
                            model,
                            *created,
                            has_tool_calls.load(Ordering::SeqCst),
                        ))),
                        _ => None,
                    }
                }
            });
            let res = Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "text/event-stream")
                .header("Cache-Control", "no-cache")
                .header("Connection", "keep-alive")
                .body(BodyExt::boxed(StreamBody::new(stream)))?;
            Ok(res)
        } else {
            let output = client.chat_completions_inner(&http_client, data).await?;
            let res = Response::builder()
                .header("Content-Type", "application/json")
                .body(
                    Full::new(ret_non_stream(
                        &completion_id,
                        &model_name,
                        created,
                        &output,
                    ))
                    .boxed(),
                )?;
            Ok(res)
        }
    }

    pub(crate) async fn ag_ui_run(
        &self,
        agent: &str,
        session: &str,
        req_body: &[u8],
    ) -> Result<AgUiAppResponse, AgUiError> {
        ag_ui::ag_ui_run_with_call_fn(&self.config, agent, session, req_body, None).await
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
struct ChatCompletionsReqBody {
    model: String,
    messages: Vec<Value>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    max_tokens: Option<isize>,
    #[serde(default)]
    stream: bool,
    tools: Option<Vec<Value>>,
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

#[derive(Debug)]
enum ResEvent {
    First(Option<String>),
    Text(String),
    ToolCalls(Vec<ToolCall>),
    Done,
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install CTRL+C signal handler")
}

fn generate_completion_id() -> String {
    let random_id = chrono::Utc::now().nanosecond();
    format!("chatcmpl-{random_id}")
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

fn create_text_frame(id: &str, model: &str, created: i64, content: &str) -> Frame<Bytes> {
    let delta = if content.is_empty() {
        json!({ "role": "assistant", "content": content })
    } else {
        json!({ "content": content })
    };
    let choice = json!({
        "index": 0,
        "delta": delta,
        "finish_reason": null,
    });
    let value = build_chat_completion_chunk_json(id, model, created, &choice);
    Frame::data(Bytes::from(format!("data: {value}\n\n")))
}

fn create_tool_calls_frame(
    id: &str,
    model: &str,
    created: i64,
    tool_calls: &[ToolCall],
) -> Frame<Bytes> {
    let chunks = tool_calls
        .iter()
        .enumerate()
        .flat_map(|(i, call)| {
            let choice1 = json!({
              "index": 0,
              "delta": {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                  {
                    "index": i,
                    "id": call.id,
                    "type": "function",
                    "function": {
                      "name": call.name,
                      "arguments": ""
                    }
                  }
                ]
              },
              "finish_reason": null
            });
            let choice2 = json!({
              "index": 0,
              "delta": {
                "tool_calls": [
                  {
                    "index": i,
                    "function": {
                      "arguments": call.arguments.to_string(),
                    }
                  }
                ]
              },
              "finish_reason": null
            });
            vec![
                build_chat_completion_chunk_json(id, model, created, &choice1),
                build_chat_completion_chunk_json(id, model, created, &choice2),
            ]
        })
        .map(|v| format!("data: {v}\n\n"))
        .collect::<Vec<String>>()
        .join("");
    Frame::data(Bytes::from(chunks))
}

fn create_done_frame(id: &str, model: &str, created: i64, has_tool_calls: bool) -> Frame<Bytes> {
    let finish_reason = if has_tool_calls { "tool_calls" } else { "stop" };
    let choice = json!({
        "index": 0,
        "delta": {},
        "finish_reason": finish_reason,
    });
    let value = build_chat_completion_chunk_json(id, model, created, &choice);
    Frame::data(Bytes::from(format!("data: {value}\n\ndata: [DONE]\n\n")))
}

fn build_chat_completion_chunk_json(id: &str, model: &str, created: i64, choice: &Value) -> Value {
    json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [choice],
    })
}

fn ret_non_stream(id: &str, model: &str, created: i64, output: &ChatCompletionsOutput) -> Bytes {
    let id = output.id.as_deref().unwrap_or(id);
    let input_tokens = output.input_tokens.unwrap_or_default();
    let output_tokens = output.output_tokens.unwrap_or_default();
    let total_tokens = input_tokens + output_tokens;
    let choice = if output.tool_calls.is_empty() {
        json!({
            "index": 0,
            "message": {
                "role": "assistant",
                "content": output.text,
            },
            "logprobs": null,
            "finish_reason": "stop",
        })
    } else {
        let content = if output.text.is_empty() {
            Value::Null
        } else {
            output.text.clone().into()
        };
        let tool_calls: Vec<_> = output
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": call.arguments.to_string(),
                    }
                })
            })
            .collect();
        json!({
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content,
                "tool_calls": tool_calls,
            },
            "logprobs": null,
            "finish_reason": "tool_calls",
        })
    };
    let res_body = json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [choice],
        "usage": {
            "prompt_tokens": input_tokens,
            "completion_tokens": output_tokens,
            "total_tokens": total_tokens,
        },
    });
    Bytes::from(res_body.to_string())
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

fn parse_agents_route(path: &str) -> Option<RouteMatch<'_>> {
    let suffix = path.strip_prefix("/v1/agents/")?;
    let segments: Vec<_> = suffix
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    match segments.as_slice() {
        [agent] => Some((*agent, None, AgentsRoute::Agent)),
        [agent, "sessions"] => Some((*agent, None, AgentsRoute::Sessions)),
        [agent, "sessions", session] => Some((*agent, Some(*session), AgentsRoute::Session)),
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
    let scoped = fork_prompt_config(config);
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
fn parse_messages(message: Vec<Value>) -> Result<Vec<Message>> {
    let mut output = vec![];
    let mut tool_results = None;
    for (i, message) in message.into_iter().enumerate() {
        let err = || anyhow!("Failed to parse '.messages[{i}]'");
        let role = message["role"].as_str().ok_or_else(err)?;
        let content = match message.get("content") {
            Some(value) => {
                if let Some(value) = value.as_str() {
                    MessageContent::Text(value.to_string())
                } else if value.is_array() {
                    let value = serde_json::from_value(value.clone()).map_err(|_| err())?;
                    MessageContent::Array(value)
                } else if value.is_null() {
                    MessageContent::Text(String::new())
                } else {
                    return Err(err());
                }
            }
            None => MessageContent::Text(String::new()),
        };
        match role {
            "system" | "user" => {
                let role = match role {
                    "system" => MessageRole::System,
                    "user" => MessageRole::User,
                    _ => unreachable!(),
                };
                output.push(Message::new(role, content))
            }
            "assistant" => {
                let role = MessageRole::Assistant;
                match message["tool_calls"].as_array() {
                    Some(tool_calls) => {
                        if tool_results.is_some() {
                            return Err(err());
                        }
                        let mut list = vec![];
                        for tool_call in tool_calls {
                            if let (id, Some(name), Some(arguments)) = (
                                tool_call["id"].as_str().map(|v| v.to_string()),
                                tool_call["function"]["name"].as_str(),
                                tool_call["function"]["arguments"].as_str(),
                            ) {
                                let arguments =
                                    serde_json::from_str(arguments).map_err(|_| err())?;
                                let thought_signature = tool_call["function"]["thought_signature"]
                                    .as_str()
                                    .map(|v| v.to_string());
                                list.push((id, name.to_string(), arguments, thought_signature));
                            } else {
                                return Err(err());
                            }
                        }
                        tool_results = Some((content.to_text(), list, vec![]));
                    }
                    None => output.push(Message::new(role, content)),
                }
            }
            "tool" => match tool_results.take() {
                Some((text, tool_calls, mut tool_values)) => {
                    let tool_call_id = message["tool_call_id"].as_str().map(|v| v.to_string());
                    let content = content.to_text();
                    let value: Value = serde_json::from_str(&content)
                        .ok()
                        .unwrap_or_else(|| content.into());

                    tool_values.push((value, tool_call_id));

                    if tool_calls.len() == tool_values.len() {
                        let mut list = vec![];
                        for ((id, name, arguments, thought_signature), (value, tool_call_id)) in
                            tool_calls.into_iter().zip(tool_values)
                        {
                            if id != tool_call_id {
                                return Err(err());
                            }
                            list.push(ToolResult::new(
                                ToolCall::new(name, arguments, id, thought_signature),
                                value,
                            ))
                        }
                        output.push(Message::new(
                            MessageRole::Assistant,
                            MessageContent::ToolCalls(MessageContentToolCalls::new(
                                list, text, None,
                            )),
                        ));
                        tool_results = None;
                    } else {
                        tool_results = Some((text, tool_calls, tool_values));
                    }
                }
                None => return Err(err()),
            },
            _ => {
                return Err(err());
            }
        }
    }

    if tool_results.is_some() {
        bail!("Invalid messages");
    }

    Ok(output)
}

fn parse_tools(tools: Option<Vec<Value>>) -> Result<Option<Vec<ToolDeclaration>>> {
    let tools = match tools {
        Some(v) => v,
        None => return Ok(None),
    };
    let mut functions = vec![];
    for (i, tool) in tools.into_iter().enumerate() {
        if let (Some("function"), Some(function)) = (
            tool["type"].as_str(),
            tool["function"]
                .as_object()
                .and_then(|v| serde_json::from_value(json!(v)).ok()),
        ) {
            functions.push(function);
        } else {
            bail!("Failed to parse '.tools[{i}]'")
        }
    }
    Ok(Some(functions))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use std::{
        fs,
        path::PathBuf,
        sync::{LazyLock, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    };

    static TEST_CONFIG_ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct TestConfigSandbox {
        _lock: std::sync::MutexGuard<'static, ()>,
        root: PathBuf,
        data_dir: PathBuf,
        state_dir: PathBuf,
        vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl TestConfigSandbox {
        fn new() -> Self {
            let lock = TEST_CONFIG_ENV_LOCK.lock().expect("config env lock");
            let root = unique_test_config_dir();
            let data_dir = root.join("data");
            let state_dir = root.join("state");
            fs::create_dir_all(root.join("clients")).expect("create clients dir");
            fs::create_dir_all(root.join("agents")).expect("create agents dir");
            fs::create_dir_all(&data_dir).expect("create data dir");
            fs::create_dir_all(&state_dir).expect("create state dir");
            fs::write(
                root.join("config.yaml"),
                "model: openai:gpt-4o\nclients: []\n",
            )
            .expect("write config");
            fs::write(
                root.join("clients/openai.yaml"),
                concat!(
                    "type: openai\n",
                    "api_key: sk-test\n",
                    "models:\n",
                    "  - name: gpt-4o\n",
                    "    type: chat\n",
                    "    max_input_tokens: 4096\n"
                ),
            )
            .expect("write openai client");

            let vars = vec![
                ("HARNX_CONFIG_DIR", std::env::var_os("HARNX_CONFIG_DIR")),
                ("HARNX_DATA_DIR", std::env::var_os("HARNX_DATA_DIR")),
                ("HARNX_STATE_DIR", std::env::var_os("HARNX_STATE_DIR")),
            ];
            unsafe {
                std::env::set_var("HARNX_CONFIG_DIR", &root);
                std::env::set_var("HARNX_DATA_DIR", &data_dir);
                std::env::set_var("HARNX_STATE_DIR", &state_dir);
                std::env::remove_var("HARNX_CONFIG_FILE");
            }

            Self {
                _lock: lock,
                root,
                data_dir,
                state_dir,
                vars,
            }
        }

        fn config(&self) -> Config {
            let prev = std::env::current_dir().expect("cwd");
            std::env::set_current_dir(&self.root).expect("switch cwd");
            let result = futures::executor::block_on(Config::init(WorkingMode::Cmd, false, vec![]));
            std::env::set_current_dir(prev).expect("restore cwd");
            result.expect("load config")
        }

        fn write_agent(&self, name: &str, prompt: &str) {
            let body = format!("---\nmodel: openai:gpt-4o\n---\n{prompt}\n");
            fs::write(self.root.join("agents").join(format!("{name}.md")), body)
                .expect("write agent");
        }
    }

    impl Drop for TestConfigSandbox {
        fn drop(&mut self) {
            for (key, previous) in &self.vars {
                match previous {
                    Some(value) => unsafe { std::env::set_var(key, value) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
            let _ = fs::remove_dir_all(&self.data_dir);
            let _ = fs::remove_dir_all(&self.state_dir);
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn unique_test_config_dir() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "harnx-serve-lib-test-{}-{timestamp}",
            std::process::id()
        ))
    }

    #[test]
    fn parse_agents_route_extracts_agent_and_session_segments() {
        assert_eq!(
            parse_agents_route("/v1/agents/hephaestus"),
            Some(("hephaestus", None, AgentsRoute::Agent))
        );
        assert_eq!(
            parse_agents_route("/v1/agents/hephaestus/sessions"),
            Some(("hephaestus", None, AgentsRoute::Sessions))
        );
        assert_eq!(
            parse_agents_route("/v1/agents/hephaestus/sessions/thread-1"),
            Some(("hephaestus", Some("thread-1"), AgentsRoute::Session))
        );
        assert_eq!(parse_agents_route("/v1/agents"), None);
        assert_eq!(parse_agents_route("/v1/agents/hephaestus/extra"), None);
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

        let prompt_config = fork_prompt_config(&config);
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
}
