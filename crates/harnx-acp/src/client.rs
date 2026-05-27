use crate::{compat::TokioCompat, AcpServerConfig, NestedAcpEvent};

use agent_client_protocol as acp;
use agent_client_protocol::schema::{
    CancelNotification, ContentBlock as AcpContentBlock, CreateTerminalRequest,
    CreateTerminalResponse, Implementation, InitializeRequest, InitializeResponse,
    KillTerminalRequest, KillTerminalResponse, LoadSessionRequest, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, ProtocolVersion, ReadTextFileRequest,
    ReadTextFileResponse, RequestPermissionRequest, RequestPermissionResponse, SessionInfoUpdate,
    SessionNotification, SessionUpdate, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
    WriteTextFileRequest, WriteTextFileResponse,
};
use anyhow::{anyhow, Context, Result};
use harnx_core::event::{
    AgentEvent, AgentSource, ContentBlock, ModelEvent, PlanEntry, ToolEvent, ToolKind, ToolStatus,
};
use serde_json::json;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::runtime::Builder;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock};
use tokio::task::LocalSet;

/// Timeout for the initial connection handshake with the ACP server.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct AcpClient {
    name: String,
    config: AcpServerConfig,
    idle_timeout: Duration,
    operation_timeout: Duration,
    connected: Arc<RwLock<bool>>,
    connection_failed: Arc<RwLock<bool>>,
    initialize_response: Arc<RwLock<Option<InitializeResponse>>>,
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    worker: Arc<Mutex<Option<AcpWorkerHandle>>>,
    chunk_forwarder: Arc<RwLock<HashMap<u64, mpsc::UnboundedSender<NestedAcpEvent>>>>,
    activity_tx: broadcast::Sender<String>,
}

#[derive(Debug, Clone, Default)]
struct SessionState {
    response_text: String,
    stop_reason: Option<String>,
}

struct AcpWorkerHandle {
    tx: mpsc::UnboundedSender<WorkerCommand>,
    join: thread::JoinHandle<()>,
    abort_tx: oneshot::Sender<()>,
    dead_rx: oneshot::Receiver<()>,
}

enum WorkerCommand {
    NewSession {
        respond_to: oneshot::Sender<Result<String>>,
    },
    Prompt {
        session_id: String,
        message: String,
        respond_to: oneshot::Sender<Result<String>>,
    },
    LoadSession {
        session_id: String,
        respond_to: oneshot::Sender<Result<()>>,
    },
    CancelSession {
        session_id: String,
        respond_to: oneshot::Sender<Result<()>>,
    },
    Shutdown {
        respond_to: oneshot::Sender<Result<()>>,
    },
}

struct AcpNotificationClient {
    agent_name: String,
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    chunk_forwarder: Arc<RwLock<HashMap<u64, mpsc::UnboundedSender<NestedAcpEvent>>>>,
    activity_tx: broadcast::Sender<String>,
}

impl AcpNotificationClient {
    fn new(
        agent_name: String,
        sessions: Arc<RwLock<HashMap<String, SessionState>>>,
        chunk_forwarder: Arc<RwLock<HashMap<u64, mpsc::UnboundedSender<NestedAcpEvent>>>>,
        activity_tx: broadcast::Sender<String>,
    ) -> Self {
        Self {
            agent_name,
            sessions,
            chunk_forwarder,
            activity_tx,
        }
    }

    async fn forward_agent_event(&self, event: AgentEvent, source: AgentSource) {
        let mut forwarders = self.chunk_forwarder.write().await;
        let mut forwarded_to_chunk = false;
        forwarders.retain(|_, tx| {
            match tx.send(NestedAcpEvent::Agent(event.clone(), Some(source.clone()))) {
                Ok(()) => {
                    forwarded_to_chunk = true;
                    true
                }
                Err(_) => false,
            }
        });

        if !forwarded_to_chunk {
            use harnx_core::sink::emit_agent_event_with_source;
            emit_agent_event_with_source(event, Some(source));
        }
    }

    async fn session_notification(&self, args: SessionNotification) -> Result<()> {
        let session_id = args.session_id.0.to_string();
        let _ = self.activity_tx.send(session_id.clone());
        let resolved_source = resolve_notification_source(&self.agent_name, &args);

        if let SessionUpdate::SessionInfoUpdate(ref info) = args.update {
            if let Some(event) = session_info_update_event(info, resolved_source.clone()) {
                self.forward_agent_event(event, resolved_source.clone())
                    .await;
            }
            return Ok(());
        }

        let is_agent_message = matches!(args.update, SessionUpdate::AgentMessageChunk(_));
        let mut message_text: Option<String> = None;
        let event: Option<AgentEvent> = match args.update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                let text = chunk_text(&chunk.content);
                if text.trim().is_empty() {
                    None
                } else {
                    message_text = Some(text.clone());
                    Some(AgentEvent::Model(ModelEvent::MessageChunk {
                        blocks: vec![ContentBlock::Text(text)],
                    }))
                }
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                let text = chunk_text(&chunk.content);
                if text.trim().is_empty() {
                    None
                } else {
                    Some(AgentEvent::Model(ModelEvent::ThoughtChunk {
                        blocks: vec![ContentBlock::Text(text)],
                    }))
                }
            }
            SessionUpdate::ToolCall(tc) => {
                let input = tc.raw_input.clone().unwrap_or(serde_json::Value::Null);
                let meta = tc.meta.as_ref().map(|m| serde_json::json!(m));
                let markdown = meta.as_ref().and_then(markdown_from_meta_value);
                Some(AgentEvent::Tool(ToolEvent::Started {
                    id: tc.tool_call_id.to_string(),
                    name: tc.title.clone(),
                    kind: ToolKind::Other,
                    markdown,
                    input,
                    locations: vec![],
                }))
            }
            SessionUpdate::ToolCallUpdate(tcu) => {
                let markdown = tcu.fields.title.clone();
                let raw_output = tcu.fields.raw_output.clone();
                let status_str = tcu.fields.status.as_ref().map(|status| {
                    serde_json::to_value(status)
                        .ok()
                        .and_then(|v| v.as_str().map(String::from))
                        .unwrap_or_else(|| format!("{:?}", status))
                });
                let is_completed = status_str.as_deref() == Some("completed");
                if is_completed {
                    if let Some(output) = raw_output {
                        Some(AgentEvent::Tool(ToolEvent::Completed {
                            id: tcu.tool_call_id.to_string(),
                            output,
                            markdown,
                        }))
                    } else {
                        Some(AgentEvent::Tool(ToolEvent::Update {
                            id: tcu.tool_call_id.to_string(),
                            markdown,
                            status: Some(ToolStatus::Completed),
                            content: None,
                        }))
                    }
                } else if markdown.is_none() && status_str.is_none() {
                    None
                } else {
                    let status = status_str.as_deref().and_then(parse_tool_status_str);
                    Some(AgentEvent::Tool(ToolEvent::Update {
                        id: tcu.tool_call_id.to_string(),
                        markdown,
                        status,
                        content: None,
                    }))
                }
            }
            SessionUpdate::Plan(p) => {
                let entries: Vec<PlanEntry> = p
                    .entries
                    .iter()
                    .map(|e| PlanEntry {
                        status: serde_json::to_value(&e.status)
                            .ok()
                            .and_then(|v| v.as_str().map(String::from))
                            .unwrap_or_else(|| format!("{:?}", e.status)),
                        content: e.content.clone(),
                    })
                    .collect();
                if entries.is_empty() {
                    None
                } else {
                    Some(AgentEvent::Plan { entries })
                }
            }
            SessionUpdate::SessionInfoUpdate(_) => unreachable!(),
            SessionUpdate::UserMessageChunk(_)
            | SessionUpdate::AvailableCommandsUpdate(_)
            | SessionUpdate::CurrentModeUpdate(_)
            | SessionUpdate::ConfigOptionUpdate(_) => None,
            other => {
                log::debug!("Unhandled ACP SessionUpdate variant: {:?}", other);
                None
            }
        };

        if let Some(event) = event {
            if is_agent_message {
                if let Some(ref chunk) = message_text {
                    let mut sessions = self.sessions.write().await;
                    let state = sessions.entry(session_id).or_default();
                    state.response_text.push_str(chunk);
                }
            }

            self.forward_agent_event(event, resolved_source).await;
        }

        Ok(())
    }

    async fn request_permission(
        &self,
        _args: RequestPermissionRequest,
    ) -> acp::Result<RequestPermissionResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn write_text_file(
        &self,
        _args: WriteTextFileRequest,
    ) -> acp::Result<WriteTextFileResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn read_text_file(
        &self,
        _args: ReadTextFileRequest,
    ) -> acp::Result<ReadTextFileResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn create_terminal(
        &self,
        _args: CreateTerminalRequest,
    ) -> acp::Result<CreateTerminalResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _args: WaitForTerminalExitRequest,
    ) -> acp::Result<WaitForTerminalExitResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn kill_terminal(&self, _args: KillTerminalRequest) -> acp::Result<KillTerminalResponse> {
        Err(acp::Error::method_not_found())
    }
}

impl AcpClient {
    pub fn new(config: AcpServerConfig) -> Self {
        let name = config.name.clone();
        let idle_timeout = Duration::from_secs(config.idle_timeout_secs);
        let operation_timeout = Duration::from_secs(config.operation_timeout_secs);
        let (activity_tx, _) = broadcast::channel(256);
        Self {
            name,
            config,
            idle_timeout,
            operation_timeout,
            connected: Arc::new(RwLock::new(false)),
            connection_failed: Arc::new(RwLock::new(false)),
            initialize_response: Arc::new(RwLock::new(None)),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            worker: Arc::new(Mutex::new(None)),
            chunk_forwarder: Arc::new(RwLock::new(HashMap::new())),
            activity_tx,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub async fn connect(&self) -> Result<()> {
        *self.connection_failed.write().await = false;

        let mut worker_guard = self.worker.lock().await;
        if let Some(w) = worker_guard.as_mut() {
            if !matches!(
                w.dead_rx.try_recv(),
                Err(oneshot::error::TryRecvError::Closed)
            ) {
                *self.connected.write().await = true;
                return Ok(());
            }
            *worker_guard = None;
            *self.connected.write().await = false;
            *self.initialize_response.write().await = None;
            self.sessions.write().await.clear();
        } else if *self.connected.read().await {
            *self.connected.write().await = false;
            *self.initialize_response.write().await = None;
            self.sessions.write().await.clear();
        }

        let (worker, ready_rx) = spawn_worker(
            self.name.clone(),
            self.config.clone(),
            self.sessions.clone(),
            self.initialize_response.clone(),
            self.chunk_forwarder.clone(),
            self.activity_tx.clone(),
        )?;

        match tokio::time::timeout(CONNECT_TIMEOUT, ready_rx).await {
            Ok(Ok(Ok(()))) => {
                *self.connected.write().await = true;
                *worker_guard = Some(worker);
                Ok(())
            }
            Ok(Ok(Err(err))) => {
                *self.connection_failed.write().await = true;
                *self.initialize_response.write().await = None;
                self.sessions.write().await.clear();
                abort_and_join_worker(worker).await;
                Err(err)
            }
            Ok(Err(_)) => {
                *self.connection_failed.write().await = true;
                *self.initialize_response.write().await = None;
                self.sessions.write().await.clear();
                abort_and_join_worker(worker).await;
                Err(anyhow!(
                    "ACP server '{}' stopped during initialization",
                    self.name
                ))
            }
            Err(_) => {
                *self.connection_failed.write().await = true;
                *self.initialize_response.write().await = None;
                self.sessions.write().await.clear();
                abort_and_join_worker(worker).await;
                Err(anyhow!(
                    "ACP server '{}' timed out during initialization",
                    self.name
                ))
            }
        }
    }

    pub async fn disconnect(&self) -> Result<()> {
        let worker = self.worker.lock().await.take();

        *self.connected.write().await = false;
        *self.connection_failed.write().await = false;
        *self.initialize_response.write().await = None;
        self.sessions.write().await.clear();

        if let Some(worker) = worker {
            let (respond_to, response_rx) = oneshot::channel();
            let _ = worker.tx.send(WorkerCommand::Shutdown { respond_to });
            let shutdown_result = match response_rx.await {
                Ok(result) => result,
                Err(_) => Ok(()),
            };
            join_worker(worker.join).await;
            shutdown_result?;
        }

        Ok(())
    }

    pub async fn session_new(&self) -> Result<String> {
        self.ensure_connected().await?;

        let (respond_to, response_rx) = oneshot::channel();
        let tx = self.worker_sender().await?;
        tx.send(WorkerCommand::NewSession { respond_to })
            .map_err(|_| anyhow!("ACP server '{}' is not connected", self.name))?;

        tokio::time::timeout(self.idle_timeout, response_rx)
            .await
            .map_err(|_| anyhow!("ACP server '{}' timed out during session/new", self.name))?
            .map_err(|_| anyhow!("ACP server '{}' disconnected during session/new", self.name))?
    }

    pub async fn session_prompt(&self, session_id: Option<&str>, message: &str) -> Result<String> {
        self.ensure_connected().await?;

        let session_id = match session_id {
            Some(session_id) => session_id.to_owned(),
            None => self.session_new().await?,
        };

        let (respond_to, response_rx) = oneshot::channel();
        let tx = self.worker_sender().await?;
        tx.send(WorkerCommand::Prompt {
            session_id: session_id.clone(),
            message: message.to_owned(),
            respond_to,
        })
        .map_err(|_| anyhow!("ACP server '{}' is not connected", self.name))?;

        let mut activity_rx = self.activity_tx.subscribe();
        let overall_timeout = tokio::time::sleep(self.operation_timeout);
        let idle_timeout = tokio::time::sleep(self.idle_timeout);
        tokio::pin!(overall_timeout);
        tokio::pin!(idle_timeout);
        tokio::pin!(response_rx);

        loop {
            tokio::select! {
                res = &mut response_rx => {
                    return res.map_err(|_| {
                        anyhow!(
                            "ACP server '{}' disconnected during session/prompt",
                            self.name
                        )
                    })?;
                }
                _ = &mut overall_timeout => {
                    return Err(anyhow!("ACP server '{}' timed out during session/prompt (overall timeout)", self.name));
                }
                _ = &mut idle_timeout => {
                    return Err(anyhow!("ACP server '{}' timed out during session/prompt (idle timeout)", self.name));
                }
                result = activity_rx.recv() => {
                    if let Ok(sid) = result {
                        if sid == session_id {
                            idle_timeout.as_mut().reset(tokio::time::Instant::now() + self.idle_timeout);
                        }
                    }
                }
            }
        }
    }

    pub async fn session_load(&self, session_id: &str) -> Result<()> {
        self.ensure_connected().await?;

        let (respond_to, response_rx) = oneshot::channel();
        let tx = self.worker_sender().await?;
        tx.send(WorkerCommand::LoadSession {
            session_id: session_id.to_owned(),
            respond_to,
        })
        .map_err(|_| anyhow!("ACP server '{}' is not connected", self.name))?;

        tokio::time::timeout(self.idle_timeout, response_rx)
            .await
            .map_err(|_| anyhow!("ACP server '{}' timed out during session/load", self.name))?
            .map_err(|_| {
                anyhow!(
                    "ACP server '{}' disconnected during session/load",
                    self.name
                )
            })?
    }

    pub async fn session_cancel(&self, session_id: &str) -> Result<()> {
        self.ensure_connected().await?;

        let (respond_to, response_rx) = oneshot::channel();
        let tx = self.worker_sender().await?;
        tx.send(WorkerCommand::CancelSession {
            session_id: session_id.to_owned(),
            respond_to,
        })
        .map_err(|_| anyhow!("ACP server '{}' is not connected", self.name))?;

        tokio::time::timeout(self.idle_timeout, response_rx)
            .await
            .map_err(|_| anyhow!("ACP server '{}' timed out during session/cancel", self.name))?
            .map_err(|_| {
                anyhow!(
                    "ACP server '{}' disconnected during session/cancel",
                    self.name
                )
            })?
    }

    pub async fn set_chunk_forwarder(&self, id: u64, tx: mpsc::UnboundedSender<NestedAcpEvent>) {
        self.chunk_forwarder.write().await.insert(id, tx);
    }

    pub async fn clear_chunk_forwarder(&self, id: u64) {
        self.chunk_forwarder.write().await.remove(&id);
    }

    async fn ensure_connected(&self) -> Result<()> {
        self.connect().await
    }

    async fn worker_sender(&self) -> Result<mpsc::UnboundedSender<WorkerCommand>> {
        self.worker
            .lock()
            .await
            .as_ref()
            .map(|worker| worker.tx.clone())
            .ok_or_else(|| anyhow!("ACP server '{}' is not connected", self.name))
    }
}

fn spawn_worker(
    name: String,
    config: AcpServerConfig,
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    initialize_response: Arc<RwLock<Option<InitializeResponse>>>,
    chunk_forwarder: Arc<RwLock<HashMap<u64, mpsc::UnboundedSender<NestedAcpEvent>>>>,
    activity_tx: broadcast::Sender<String>,
) -> Result<(AcpWorkerHandle, oneshot::Receiver<Result<()>>)> {
    let (tx, rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (abort_tx, abort_rx) = oneshot::channel();
    let (dead_tx, dead_rx) = oneshot::channel::<()>();
    let thread_name = format!("acp-client-{name}");
    let config_name = config.name.clone();

    let join = thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let _dead_tx = dead_tx;
            let runtime = match Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(err) => {
                    let _ = ready_tx.send(Err(anyhow!(
                        "Failed to create ACP runtime for '{}': {err}",
                        name
                    )));
                    return;
                }
            };

            let local_set = LocalSet::new();
            let result = local_set.block_on(&runtime, async move {
                worker_main(
                    name,
                    config,
                    sessions,
                    initialize_response,
                    rx,
                    ready_tx,
                    chunk_forwarder,
                    abort_rx,
                    activity_tx,
                )
                .await
            });

            if let Err(err) = result {
                log::warn!("ACP worker exited with error: {err}");
            }
        })
        .with_context(|| format!("Failed to start ACP worker thread for '{}'", config_name))?;

    Ok((
        AcpWorkerHandle {
            tx,
            join,
            abort_tx,
            dead_rx,
        },
        ready_rx,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn worker_main(
    name: String,
    config: AcpServerConfig,
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    initialize_response: Arc<RwLock<Option<InitializeResponse>>>,
    mut rx: mpsc::UnboundedReceiver<WorkerCommand>,
    ready_tx: oneshot::Sender<Result<()>>,
    chunk_forwarder: Arc<RwLock<HashMap<u64, mpsc::UnboundedSender<NestedAcpEvent>>>>,
    mut abort_rx: oneshot::Receiver<()>,
    activity_tx: broadcast::Sender<String>,
) -> Result<()> {
    let mut cmd = Command::new(&config.command);
    cmd.args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(&config.env)
        .kill_on_drop(true);

    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn ACP server '{}'", name))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("ACP server '{}' did not provide stdout", name))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("ACP server '{}' did not provide stdin", name))?;

    if let Some(stderr) = child.stderr.take() {
        let server_name = name.clone();
        tokio::task::spawn_local(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                log::debug!("[acp:{}] {}", server_name, line);
            }
        });
    }

    let transport = acp::ByteStreams::new(TokioCompat::new(stdin), TokioCompat::new(stdout));
    let client = Arc::new(AcpNotificationClient::new(
        name.clone(),
        sessions.clone(),
        chunk_forwarder,
        activity_tx,
    ));
    let child = Arc::new(tokio::sync::Mutex::new(Some(child)));

    let result = acp::Client
        .builder()
        .on_receive_notification(
            {
                let client = Arc::clone(&client);
                async move |notification: SessionNotification, _cx| {
                    client
                        .session_notification(notification)
                        .await
                        .map_err(|_err| acp::Error::internal_error())
                }
            },
            acp::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let client = Arc::clone(&client);
                async move |request: RequestPermissionRequest, responder, _cx| {
                    let response = client.request_permission(request).await?;
                    responder.respond(response)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let client = Arc::clone(&client);
                async move |request: WriteTextFileRequest, responder, _cx| {
                    let response = client.write_text_file(request).await?;
                    responder.respond(response)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let client = Arc::clone(&client);
                async move |request: ReadTextFileRequest, responder, _cx| {
                    let response = client.read_text_file(request).await?;
                    responder.respond(response)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let client = Arc::clone(&client);
                async move |request: CreateTerminalRequest, responder, _cx| {
                    let response = client.create_terminal(request).await?;
                    responder.respond(response)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let client = Arc::clone(&client);
                async move |request: WaitForTerminalExitRequest, responder, _cx| {
                    let response = client.wait_for_terminal_exit(request).await?;
                    responder.respond(response)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let client = Arc::clone(&client);
                async move |request: KillTerminalRequest, responder, _cx| {
                    let response = client.kill_terminal(request).await?;
                    responder.respond(response)
                }
            },
            acp::on_receive_request!(),
        )
        .connect_with(transport, async move |connection: acp::ConnectionTo<acp::Agent>| {
            let init_fut = connection.send_request(
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("harnx", env!("CARGO_PKG_VERSION")).title("Harnx")),
            );
            let init = tokio::select! {
                result = init_fut.block_task() => {
                    result.with_context(|| format!("Failed to initialize ACP server '{}'", name))?
                }
                _ = &mut abort_rx => {
                    let _ = shutdown_child(&child).await;
                    let _ = ready_tx.send(Err(anyhow!("ACP server '{}' initialization aborted", name)));
                    return Ok(());
                }
            };

            *initialize_response.write().await = Some(init);
            let _ = ready_tx.send(Ok(()));

            loop {
                tokio::select! {
                    command = rx.recv() => {
                        match command {
                            Some(command) => match command {
                                WorkerCommand::NewSession { respond_to } => {
                                    let sessions = sessions.clone();
                                    let server_name = name.clone();
                                    let connection = connection.clone();
                                    let task_connection = connection.clone();
                                    connection.spawn(async move {
                                        let result = async {
                                            let response: NewSessionResponse = task_connection
                                                .send_request(NewSessionRequest::new(std::env::current_dir()?))
                                                .block_task()
                                                .await
                                                .with_context(|| {
                                                    format!("Failed to create ACP session on '{}'", server_name)
                                                })?;
                                            let session_id = response.session_id.0.to_string();
                                            sessions
                                                .write()
                                                .await
                                                .insert(session_id.clone(), SessionState::default());
                                            Ok(session_id)
                                        }
                                        .await;
                                        let _ = respond_to.send(result);
                                        Ok(())
                                    })?;
                                }
                                WorkerCommand::Prompt {
                                    session_id,
                                    message,
                                    respond_to,
                                } => {
                                    let sessions = sessions.clone();
                                    let server_name = name.clone();
                                    let connection = connection.clone();
                                    let task_connection = connection.clone();
                                    connection.spawn(async move {
                                        let result = async {
                                            {
                                                let mut sessions = sessions.write().await;
                                                let state = sessions.entry(session_id.clone()).or_default();
                                                state.response_text.clear();
                                                state.stop_reason = None;
                                            }

                                            let response: PromptResponse = task_connection
                                                .send_request(PromptRequest::new(
                                                    session_id.clone(),
                                                    vec![message.into()],
                                                ))
                                                .block_task()
                                                .await
                                                .with_context(|| {
                                                    format!(
                                                        "Failed to send ACP prompt to session '{}' on '{}'",
                                                        session_id, server_name
                                                    )
                                                })?;

                                            let mut sessions = sessions.write().await;
                                            let state = sessions.entry(session_id.clone()).or_default();
                                            state.stop_reason = Some(format!("{:?}", response.stop_reason));
                                            Ok(state.response_text.clone())
                                        }
                                        .await;
                                        let _ = respond_to.send(result);
                                        Ok(())
                                    })?;
                                }
                                WorkerCommand::LoadSession {
                                    session_id,
                                    respond_to,
                                } => {
                                    let sessions = sessions.clone();
                                    let server_name = name.clone();
                                    let connection = connection.clone();
                                    let task_connection = connection.clone();
                                    connection.spawn(async move {
                                        let result = async {
                                            let _response = task_connection
                                                .send_request(LoadSessionRequest::new(
                                                    session_id.clone(),
                                                    std::env::current_dir()?,
                                                ))
                                                .block_task()
                                                .await
                                                .with_context(|| {
                                                    format!(
                                                        "Failed to load ACP session '{}' on '{}'",
                                                        session_id, server_name
                                                    )
                                                })?;

                                            sessions.write().await.entry(session_id).or_default();
                                            Ok(())
                                        }
                                        .await;
                                        let _ = respond_to.send(result);
                                        Ok(())
                                    })?;
                                }
                                WorkerCommand::CancelSession {
                                    session_id,
                                    respond_to,
                                } => {
                                    let server_name = name.clone();
                                    let connection = connection.clone();
                                    let task_connection = connection.clone();
                                    connection.spawn(async move {
                                        let result = task_connection
                                            .send_notification(CancelNotification::new(session_id.clone()))
                                            .with_context(|| {
                                                format!(
                                                    "Failed to cancel ACP session '{}' on '{}'",
                                                    session_id, server_name
                                                )
                                            });
                                        let _ = respond_to.send(result);
                                        Ok(())
                                    })?;
                                }
                                WorkerCommand::Shutdown { respond_to } => {
                                    let result = shutdown_child(&child).await;
                                    let _ = respond_to.send(result);
                                    break;
                                }
                            },
                            None => break,
                        }
                    }
                    _ = &mut abort_rx => {
                        let _ = shutdown_child(&child).await;
                        break;
                    }
                }
            }

            Ok(())
        })
        .await;

    Ok(result.map(|_| ())?)
}

async fn shutdown_child(child: &Arc<tokio::sync::Mutex<Option<Child>>>) -> Result<()> {
    let mut child_guard = child.lock().await;
    if let Some(mut child) = child_guard.take() {
        if let Err(err) = child.kill().await {
            if err.kind() != std::io::ErrorKind::InvalidInput {
                return Err(err).context("Failed to kill ACP subprocess");
            }
        }
        let _ = child.wait().await;
    }
    Ok(())
}

async fn abort_and_join_worker(worker: AcpWorkerHandle) {
    let AcpWorkerHandle {
        tx,
        join,
        abort_tx,
        dead_rx: _,
    } = worker;
    let _ = abort_tx.send(());
    drop(tx);
    join_worker(join).await;
}

async fn join_worker(join: thread::JoinHandle<()>) {
    let join_result = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || {
            let _ = join.join();
        }),
    )
    .await;

    match join_result {
        Ok(blocking_result) => {
            let _ = blocking_result;
        }
        Err(_) => {
            log::warn!("Timed out waiting for ACP worker thread to exit");
        }
    }
}

fn agent_from_meta_value(value: &serde_json::Value) -> Option<String> {
    value
        .get("agent")
        .and_then(serde_json::Value::as_str)
        .filter(|agent| !agent.is_empty())
        .map(ToOwned::to_owned)
}

fn session_from_meta_value(value: &serde_json::Value) -> Option<String> {
    value
        .get("session")
        .and_then(serde_json::Value::as_str)
        .filter(|session| !session.is_empty())
        .map(ToOwned::to_owned)
}

fn model_from_meta_value(value: &serde_json::Value) -> Option<String> {
    value
        .get("harnx:model")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn markdown_from_meta_value(value: &serde_json::Value) -> Option<String> {
    value
        .get("harnx:markdown")
        .and_then(serde_json::Value::as_str)
        .filter(|t| !t.is_empty())
        .map(ToOwned::to_owned)
}

fn resolve_notification_source(
    fallback_agent: &str,
    notification: &SessionNotification,
) -> AgentSource {
    let session_id = notification.session_id.0.to_string();
    let (update_agent, update_session, update_model) = match &notification.update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            let meta = chunk.meta.as_ref().map(|meta| json!(meta));
            (
                meta.as_ref().and_then(agent_from_meta_value),
                meta.as_ref().and_then(session_from_meta_value),
                meta.as_ref().and_then(model_from_meta_value),
            )
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            let meta = chunk.meta.as_ref().map(|meta| json!(meta));
            (
                meta.as_ref().and_then(agent_from_meta_value),
                meta.as_ref().and_then(session_from_meta_value),
                meta.as_ref().and_then(model_from_meta_value),
            )
        }
        SessionUpdate::ToolCall(call) => {
            let meta = call.meta.as_ref().map(|meta| json!(meta));
            (
                meta.as_ref().and_then(agent_from_meta_value),
                meta.as_ref().and_then(session_from_meta_value),
                meta.as_ref().and_then(model_from_meta_value),
            )
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let meta = update.meta.as_ref().map(|meta| json!(meta));
            (
                meta.as_ref().and_then(agent_from_meta_value),
                meta.as_ref().and_then(session_from_meta_value),
                meta.as_ref().and_then(model_from_meta_value),
            )
        }
        SessionUpdate::Plan(plan) => {
            let meta = plan.meta.as_ref().map(|meta| json!(meta));
            (
                meta.as_ref().and_then(agent_from_meta_value),
                meta.as_ref().and_then(session_from_meta_value),
                meta.as_ref().and_then(model_from_meta_value),
            )
        }
        SessionUpdate::SessionInfoUpdate(info) => {
            let direct_meta = info.meta.as_ref().map(|meta| json!(meta));
            (
                direct_meta
                    .as_ref()
                    .and_then(agent_from_meta_value)
                    .or_else(|| {
                        info.meta
                            .as_ref()
                            .and_then(|meta| meta.get("harnx:usage"))
                            .and_then(agent_from_meta_value)
                    }),
                direct_meta
                    .as_ref()
                    .and_then(session_from_meta_value)
                    .or_else(|| {
                        info.meta
                            .as_ref()
                            .and_then(|meta| meta.get("harnx:usage"))
                            .and_then(session_from_meta_value)
                    }),
                direct_meta
                    .as_ref()
                    .and_then(model_from_meta_value)
                    .or_else(|| {
                        info.meta
                            .as_ref()
                            .and_then(|meta| meta.get("harnx:usage"))
                            .and_then(model_from_meta_value)
                    }),
            )
        }
        _ => (None, None, None),
    };

    AgentSource {
        agent: update_agent.unwrap_or_else(|| fallback_agent.to_string()),
        session_id: Some(update_session.unwrap_or(session_id)),
        model: update_model,
    }
}

fn session_info_update_event(info: &SessionInfoUpdate, source: AgentSource) -> Option<AgentEvent> {
    let meta = info.meta.as_ref()?;
    let usage = meta.get("harnx:usage")?;
    let input_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
    let output_tokens = usage["output_tokens"].as_u64().unwrap_or(0);
    let cached_tokens = usage["cached_tokens"].as_u64().unwrap_or(0);
    if input_tokens == 0 && output_tokens == 0 && cached_tokens == 0 {
        return None;
    }

    let session_label = Some(source_heading(&source));

    Some(AgentEvent::Model(ModelEvent::Usage {
        input: input_tokens,
        output: output_tokens,
        cached: cached_tokens,
        session_label,
    }))
}

fn parse_tool_status_str(status: &str) -> Option<ToolStatus> {
    match status {
        "pending" | "Pending" => Some(ToolStatus::Pending),
        "in_progress" | "InProgress" | "in-progress" => Some(ToolStatus::InProgress),
        "completed" | "Completed" => Some(ToolStatus::Completed),
        "failed" | "Failed" => Some(ToolStatus::Failed),
        _ => None,
    }
}

fn source_heading(source: &AgentSource) -> String {
    source.heading()
}

fn chunk_text(content: &AcpContentBlock) -> String {
    match content {
        AcpContentBlock::Text(text) => text.text.clone(),
        AcpContentBlock::ResourceLink(link) => link.uri.to_string(),
        AcpContentBlock::Image(_) => "<image>".to_string(),
        AcpContentBlock::Audio(_) => "<audio>".to_string(),
        AcpContentBlock::Resource(_) => "<resource>".to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::{
        ContentChunk, SessionId, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    };
    use serde_json::json;

    #[test]
    fn session_info_update_event_emits_usage_model_event() {
        let info = SessionInfoUpdate::new().meta(serde_json::Map::from_iter([(
            "harnx:usage".to_string(),
            json!({
                "agent": "aristarchus",
                "session": "nested-session-1",
                "input_tokens": 10,
                "output_tokens": 2,
                "cached_tokens": 5,
            }),
        )]));

        let event = session_info_update_event(
            &info,
            AgentSource {
                agent: "fallback-agent".to_string(),
                session_id: Some("outer-session-1".to_string()),
                model: None,
            },
        )
        .expect("usage event");
        match event {
            AgentEvent::Model(ModelEvent::Usage {
                input,
                output,
                cached,
                session_label,
            }) => {
                assert_eq!(input, 10);
                assert_eq!(output, 2);
                assert_eq!(cached, 5);
                assert!(session_label.is_some());
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn resolve_notification_source_falls_back_to_client_name() {
        let notification = SessionNotification::new(
            SessionId::new("outer-session"),
            SessionUpdate::AgentMessageChunk(ContentChunk::new("hello".into())),
        );

        let source = resolve_notification_source("argus", &notification);
        assert_eq!(source.agent, "argus");
        assert_eq!(source.session_id.as_deref(), Some("outer-session"));
    }

    #[test]
    fn resolve_notification_source_uses_nested_session_when_present() {
        let notification = SessionNotification::new(
            SessionId::new("outer-session"),
            SessionUpdate::AgentMessageChunk(ContentChunk::new("hello".into()).meta(
                serde_json::Map::from_iter([
                    ("agent".to_string(), json!("aristarchus")),
                    ("session".to_string(), json!("nested-session")),
                ]),
            )),
        );

        let source = resolve_notification_source("argus", &notification);
        assert_eq!(source.agent, "aristarchus");
        assert_eq!(source.session_id.as_deref(), Some("nested-session"));
    }

    #[test]
    fn resolve_notification_source_uses_nested_tool_call_metadata() {
        let notification = SessionNotification::new(
            SessionId::new("outer-session"),
            SessionUpdate::ToolCall(ToolCall::new("ls", "path: /tmp").meta(
                serde_json::Map::from_iter([
                    ("agent".to_string(), json!("pytheas")),
                    (
                        "session".to_string(),
                        json!("608e48b6-c880-4168-b028-1bda3469be07"),
                    ),
                ]),
            )),
        );

        let source = resolve_notification_source("working", &notification);
        assert_eq!(source.agent, "pytheas");
        assert_eq!(
            source.session_id.as_deref(),
            Some("608e48b6-c880-4168-b028-1bda3469be07")
        );
    }

    #[tokio::test]
    async fn worker_death_triggers_reconnection_attempt() {
        let config = AcpServerConfig {
            name: "mock-dead".to_string(),
            command: "__harnx_test_nonexistent_binary__".to_string(),
            args: vec![],
            env: HashMap::new(),
            enabled: true,
            description: None,
            idle_timeout_secs: 5,
            operation_timeout_secs: 30,
            package: None,
        };
        let client = AcpClient::new(config);

        let (tx, _rx) = mpsc::unbounded_channel::<WorkerCommand>();
        let (abort_tx, _abort_rx) = oneshot::channel::<()>();
        let (dead_tx, dead_rx) = oneshot::channel::<()>();
        drop(dead_tx);

        let join = thread::spawn(|| {});

        let stale_handle = AcpWorkerHandle {
            tx,
            join,
            abort_tx,
            dead_rx,
        };

        *client.worker.lock().await = Some(stale_handle);
        *client.connected.write().await = true;

        let result = client.ensure_connected().await;

        assert!(
            !*client.connected.read().await,
            "connected must be reset after dead worker detected"
        );
        assert!(
            client.sessions.read().await.is_empty(),
            "sessions must be cleared after dead worker detected"
        );
        assert!(
            client.initialize_response.read().await.is_none(),
            "initialize_response must be cleared after dead worker detected"
        );
        assert!(
            client.worker.lock().await.is_none(),
            "worker must be cleared after dead worker detected"
        );
        assert!(
            result.is_err(),
            "ensure_connected should return an error when the binary does not exist"
        );
    }

    #[tokio::test]
    async fn nested_tool_call_notification_preserves_structured_event_for_tui_pipeline() {
        let sessions = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let chunk_forwarder = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel();
        chunk_forwarder.write().await.insert(1, chunk_tx);
        let (activity_tx, _) = tokio::sync::broadcast::channel(8);
        let client = AcpNotificationClient::new(
            "working".to_string(),
            sessions,
            chunk_forwarder,
            activity_tx,
        );

        let notification = SessionNotification::new(
            SessionId::new("outer-session"),
            SessionUpdate::ToolCall(
                ToolCall::new("call-1", "pytheas_session_prompt")
                    .raw_input(json!({
                        "message": "Count files in /tmp using ls first.",
                        "session_id": "608e48b6-c880-4168-b028-1bda3469be07",
                    }))
                    .meta(serde_json::Map::from_iter([
                        ("agent".to_string(), json!("pytheas")),
                        (
                            "session".to_string(),
                            json!("608e48b6-c880-4168-b028-1bda3469be07"),
                        ),
                    ])),
            ),
        );

        client.session_notification(notification).await.unwrap();

        let forwarded = chunk_rx.recv().await.expect("forwarded nested ACP event");
        let (forwarded_event, forwarded_source) = match forwarded {
            NestedAcpEvent::Agent(event, source) => (event, source),
            other => panic!("unexpected nested ACP event: {other:?}"),
        };

        match forwarded_event {
            AgentEvent::Tool(ToolEvent::Started {
                id,
                name,
                input,
                kind,
                ..
            }) => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "pytheas_session_prompt");
                assert!(matches!(kind, ToolKind::Other));
                assert_eq!(
                    input,
                    json!({
                        "message": "Count files in /tmp using ls first.",
                        "session_id": "608e48b6-c880-4168-b028-1bda3469be07",
                    })
                );
            }
            other => panic!("unexpected forwarded event: {other:?}"),
        }

        let source = forwarded_source.expect("nested source should be preserved");
        assert_eq!(source.agent, "pytheas");
        assert_eq!(
            source.session_id.as_deref(),
            Some("608e48b6-c880-4168-b028-1bda3469be07")
        );
    }

    #[tokio::test]
    async fn nested_tool_call_completed_event_preserves_structured_output_for_tui_pipeline() {
        let sessions = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let chunk_forwarder = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel();
        chunk_forwarder.write().await.insert(1, chunk_tx);
        let (activity_tx, _) = tokio::sync::broadcast::channel(8);
        let client = AcpNotificationClient::new(
            "working".to_string(),
            sessions,
            chunk_forwarder,
            activity_tx,
        );

        let expected_output = json!({"result": "file.txt"});
        let notification = SessionNotification::new(
            SessionId::new("outer-session"),
            SessionUpdate::ToolCallUpdate(
                ToolCallUpdate::new(
                    "call-1",
                    ToolCallUpdateFields::new()
                        .status(ToolCallStatus::Completed)
                        .raw_output(expected_output.clone())
                        .title("done"),
                )
                .meta(serde_json::Map::from_iter([
                    ("agent".to_string(), json!("pytheas")),
                    (
                        "session".to_string(),
                        json!("608e48b6-c880-4168-b028-1bda3469be07"),
                    ),
                ])),
            ),
        );

        client.session_notification(notification).await.unwrap();

        let forwarded = chunk_rx.recv().await.expect("forwarded nested ACP event");
        let (forwarded_event, forwarded_source) = match forwarded {
            NestedAcpEvent::Agent(event, source) => (event, source),
            other => panic!("unexpected nested ACP event: {other:?}"),
        };

        match forwarded_event {
            AgentEvent::Tool(ToolEvent::Completed {
                id,
                output,
                markdown,
            }) => {
                assert_eq!(id, "call-1");
                assert_eq!(output, expected_output);
                assert_eq!(markdown.as_deref(), Some("done"));
            }
            other => panic!("unexpected forwarded event: {other:?}"),
        }

        let source = forwarded_source.expect("nested source should be preserved");
        assert_eq!(source.agent, "pytheas");
        assert_eq!(
            source.session_id.as_deref(),
            Some("608e48b6-c880-4168-b028-1bda3469be07")
        );
    }

    #[tokio::test]
    async fn nested_usage_session_info_update_emits_usage_event_without_message_chunk() {
        let sessions = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let chunk_forwarder = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel();
        chunk_forwarder.write().await.insert(1, chunk_tx);
        let (activity_tx, _) = tokio::sync::broadcast::channel(8);
        let client = AcpNotificationClient::new(
            "working".to_string(),
            sessions.clone(),
            chunk_forwarder,
            activity_tx,
        );

        let notification = SessionNotification::new(
            SessionId::new("outer-session"),
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().meta(
                serde_json::Map::from_iter([(
                    "harnx:usage".to_string(),
                    json!({
                        "agent": "pytheas",
                        "session": "608e48b6-c880-4168-b028-1bda3469be07",
                        "input_tokens": 11,
                        "output_tokens": 7,
                        "cached_tokens": 3,
                    }),
                )]),
            )),
        );

        client.session_notification(notification).await.unwrap();

        let forwarded = chunk_rx.recv().await.expect("forwarded nested ACP event");
        let (forwarded_event, forwarded_source) = match forwarded {
            NestedAcpEvent::Agent(event, source) => (event, source),
            other => panic!("unexpected nested ACP event: {other:?}"),
        };

        match forwarded_event {
            AgentEvent::Model(ModelEvent::Usage {
                input,
                output,
                cached,
                session_label,
            }) => {
                assert_eq!(input, 11);
                assert_eq!(output, 7);
                assert_eq!(cached, 3);
                assert_eq!(
                    session_label.as_deref(),
                    Some("> pytheas ▸ 608e48b6-c880-4168-b028-1bda3469be07")
                );
            }
            other => panic!("unexpected forwarded event: {other:?}"),
        }

        let source = forwarded_source.expect("nested source should be preserved");
        assert_eq!(source.agent, "pytheas");
        assert_eq!(
            source.session_id.as_deref(),
            Some("608e48b6-c880-4168-b028-1bda3469be07")
        );
        assert!(
            sessions.read().await.is_empty(),
            "usage update must not create/append response text state"
        );
    }

    #[tokio::test]
    async fn tool_call_update_completed_without_output_emits_completed_update_event() {
        let sessions = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let chunk_forwarder = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel();
        chunk_forwarder.write().await.insert(1, chunk_tx);
        let (activity_tx, _) = tokio::sync::broadcast::channel(8);
        let client = AcpNotificationClient::new(
            "working".to_string(),
            sessions,
            chunk_forwarder,
            activity_tx,
        );

        let notification = SessionNotification::new(
            SessionId::new("outer-session"),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "call-no-output",
                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
            )),
        );

        client
            .session_notification(notification)
            .await
            .expect("session notification should succeed");

        let NestedAcpEvent::Agent(forwarded_event, _) =
            chunk_rx.recv().await.expect("forwarded nested ACP event")
        else {
            panic!("unexpected nested ACP event");
        };

        match forwarded_event {
            AgentEvent::Tool(ToolEvent::Update { id, status, .. }) => {
                assert_eq!(id, "call-no-output");
                assert!(
                    matches!(status, Some(ToolStatus::Completed)),
                    "expected ToolStatus::Completed, got {status:?}"
                );
            }
            other => panic!("expected ToolEvent::Update with Completed status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn nested_tool_call_markdown_round_trips_through_acp_meta() {
        let sessions = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let chunk_forwarder = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel();
        chunk_forwarder.write().await.insert(1, chunk_tx);
        let (activity_tx, _) = tokio::sync::broadcast::channel(8);
        let client = AcpNotificationClient::new(
            "working".to_string(),
            sessions,
            chunk_forwarder,
            activity_tx,
        );

        let notification = SessionNotification::new(
            SessionId::new("outer-session"),
            SessionUpdate::ToolCall(
                ToolCall::new("call-1", "bash_exec")
                    .raw_input(json!({"command": "ls -la /tmp"}))
                    .meta(serde_json::Map::from_iter([
                        ("agent".to_string(), json!("pytheas")),
                        ("harnx:markdown".to_string(), json!("**$** `ls -la /tmp`")),
                    ])),
            ),
        );

        client.session_notification(notification).await.unwrap();

        let forwarded = chunk_rx.recv().await.expect("forwarded nested ACP event");
        let (forwarded_event, _) = match forwarded {
            NestedAcpEvent::Agent(event, source) => (event, source),
            other => panic!("unexpected nested ACP event: {other:?}"),
        };

        match &forwarded_event {
            AgentEvent::Tool(ToolEvent::Started { markdown, .. }) => {
                assert_eq!(
                    markdown.as_deref(),
                    Some("**$** `ls -la /tmp`"),
                    "markdown should round-trip through ACP meta"
                );
            }
            other => panic!("unexpected forwarded event: {other:?}"),
        }
    }
}
