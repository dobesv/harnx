use crate::{AcpServerConfig, NestedAcpEvent};

use agent_client_protocol::{self as acp, Agent as _};
use anyhow::{anyhow, Context, Result};
use harnx_core::event::{
    AgentEvent, AgentSource, ContentBlock, ModelEvent, PlanEntry, ToolEvent, ToolKind, ToolStatus,
};
use serde_json::json;
use std::cell::RefCell;
use std::collections::HashMap;
use std::pin::Pin;
use std::process::Stdio;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::thread;
use std::time::Duration;
use tokio::io::{
    AsyncBufReadExt, AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, BufReader, ReadBuf,
};
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
    initialize_response: Arc<RwLock<Option<acp::InitializeResponse>>>,
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

struct TokioCompat<T> {
    inner: T,
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

        // Prefer delivery through a registered chunk forwarder when one is
        // present.  Registered forwarders (e.g. `forward_acp_chunks` in
        // `tool.rs` for the parent TUI, or `forward_task` in `server.rs` for
        // nested ACP relay) already know how to surface the event to the
        // right destination — emitting directly here would cause the same
        // event to appear twice in the parent transcript.
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

        // When no forwarder is registered (e.g. direct parent TUI mode with
        // no tool call in flight), fall back to the unified AgentEvent sink
        // so the event still reaches the transcript.
        if !forwarded_to_chunk {
            use harnx_core::sink::emit_agent_event_with_source;
            emit_agent_event_with_source(event, Some(source));
        }
    }
}

impl<T> TokioCompat<T> {
    fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T: TokioAsyncRead + Unpin> futures_util::io::AsyncRead for TokioCompat<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut read_buf = ReadBuf::new(buf);
        match Pin::new(&mut self.inner).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: TokioAsyncWrite + Unpin> futures_util::io::AsyncWrite for TokioCompat<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for AcpNotificationClient {
    async fn request_permission(
        &self,
        _args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        let session_id = args.session_id.0.to_string();
        let _ = self.activity_tx.send(session_id.clone());
        let resolved_source = resolve_notification_source(&self.agent_name, &args);

        // Handle SessionInfoUpdate separately: its display output is
        // display-only metadata (e.g. usage banners) that must never be
        // appended to `state.response_text`.  When chunks are re-emitted
        // upward by server.rs they become AgentMessageChunk, which *would*
        // trigger the response_text append in the parent.  By handling
        // SessionInfoUpdate with an early return we keep it out of the
        // shared chunk → response_text path entirely.
        if let acp::SessionUpdate::SessionInfoUpdate(ref info) = args.update {
            if let Some(event) = session_info_update_event(info, resolved_source.clone()) {
                self.forward_agent_event(event, resolved_source.clone())
                    .await;
            }
            return Ok(());
        }

        let is_agent_message = matches!(args.update, acp::SessionUpdate::AgentMessageChunk(_));
        // `message_text` records the streamed text for session
        // response_text appending when the update is an AgentMessageChunk.
        let mut message_text: Option<String> = None;
        let event: Option<AgentEvent> = match args.update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
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
            acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                let text = chunk_text(&chunk.content);
                if text.trim().is_empty() {
                    None
                } else {
                    Some(AgentEvent::Model(ModelEvent::ThoughtChunk {
                        blocks: vec![ContentBlock::Text(text)],
                    }))
                }
            }
            acp::SessionUpdate::ToolCall(tc) => {
                let input = tc.raw_input.clone().unwrap_or(serde_json::Value::Null);
                Some(AgentEvent::Tool(ToolEvent::Started {
                    id: tc.tool_call_id.to_string(),
                    name: tc.title.clone(),
                    kind: ToolKind::Other,
                    title: None,
                    input,
                    locations: vec![],
                }))
            }
            acp::SessionUpdate::ToolCallUpdate(tcu) => {
                let title = tcu.fields.title.clone();
                let status_str = tcu.fields.status.as_ref().map(|status| {
                    serde_json::to_value(status)
                        .ok()
                        .and_then(|v| v.as_str().map(String::from))
                        .unwrap_or_else(|| format!("{:?}", status))
                });
                if title.is_none() && status_str.is_none() {
                    None
                } else {
                    let status = status_str.as_deref().and_then(parse_tool_status_str);
                    Some(AgentEvent::Tool(ToolEvent::Update {
                        id: tcu.tool_call_id.to_string(),
                        title,
                        status,
                        content: None,
                    }))
                }
            }
            acp::SessionUpdate::Plan(p) => {
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
            // SessionInfoUpdate is handled above via early return.
            acp::SessionUpdate::SessionInfoUpdate(_) => unreachable!(),
            // Explicitly list known-but-unhandled variants so new ones from
            // future ACP SDK upgrades surface as compile warnings in the
            // wildcard arm below.
            acp::SessionUpdate::UserMessageChunk(_)
            | acp::SessionUpdate::AvailableCommandsUpdate(_)
            | acp::SessionUpdate::CurrentModeUpdate(_)
            | acp::SessionUpdate::ConfigOptionUpdate(_) => None,
            // Required catch-all: SessionUpdate is #[non_exhaustive].
            // Log so future variants aren't silently swallowed.
            other => {
                log::debug!("Unhandled ACP SessionUpdate variant: {:?}", other);
                None
            }
        };

        if let Some(event) = event {
            let chunk_for_response = if is_agent_message {
                message_text.clone()
            } else {
                None
            };

            self.forward_agent_event(event, resolved_source.clone())
                .await;

            if let Some(chunk) = chunk_for_response {
                let mut sessions = self.sessions.write().await;
                let state = sessions.entry(session_id).or_default();
                state.response_text.push_str(&chunk);
            }
        }

        Ok(())
    }

    async fn write_text_file(
        &self,
        _args: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn read_text_file(
        &self,
        _args: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn create_terminal(
        &self,
        _args: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn terminal_output(
        &self,
        _args: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _args: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _args: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn kill_terminal(
        &self,
        _args: acp::KillTerminalRequest,
    ) -> acp::Result<acp::KillTerminalResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_method(&self, _args: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_notification(&self, _args: acp::ExtNotification) -> acp::Result<()> {
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
                // Worker is still alive (Err(Empty)) or already connected — trust it.
                *self.connected.write().await = true;
                return Ok(());
            }
            // Worker died (Err(Closed)) — clear stale state and fall through to re-spawn.
            *worker_guard = None;
            *self.connected.write().await = false;
            *self.initialize_response.write().await = None;
            self.sessions.write().await.clear();
        } else if *self.connected.read().await {
            // connected=true but no worker — shouldn't happen; reset fully and reconnect.
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
        // Delegate entirely to connect(), which holds the worker mutex for its
        // full body (liveness probe → state clear → spawn → store).  This
        // means concurrent callers serialize naturally: the second caller enters
        // connect() after the first has already stored a fresh worker handle,
        // sees it alive, and returns Ok(()) without spawning a duplicate.
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
    initialize_response: Arc<RwLock<Option<acp::InitializeResponse>>>,
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
    initialize_response: Arc<RwLock<Option<acp::InitializeResponse>>>,
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

    let client =
        AcpNotificationClient::new(name.clone(), sessions.clone(), chunk_forwarder, activity_tx);
    let (conn, handle_io) = acp::ClientSideConnection::new(
        client,
        TokioCompat::new(stdin),
        TokioCompat::new(stdout),
        |future| {
            tokio::task::spawn_local(future);
        },
    );
    let conn = Rc::new(conn);

    tokio::task::spawn_local(async move {
        if let Err(err) = handle_io.await {
            log::debug!("ACP I/O loop exited: {err}");
        }
    });

    let init = tokio::select! {
        result = conn.initialize(
            acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_info(
                acp::Implementation::new("harnx", env!("CARGO_PKG_VERSION")).title("Harnx"),
            ),
        ) => {
            result.with_context(|| format!("Failed to initialize ACP server '{}'", name))?
        }
        _ = &mut abort_rx => {
            if let Err(err) = child.kill().await {
                if err.kind() != std::io::ErrorKind::InvalidInput {
                    return Err(err).context("Failed to kill ACP subprocess");
                }
            }
            let _ = child.wait().await;
            let _ = ready_tx.send(Err(anyhow!("ACP server '{}' initialization aborted", name)));
            return Ok(());
        }
    };

    *initialize_response.write().await = Some(init);
    let _ = ready_tx.send(Ok(()));

    let child = Rc::new(RefCell::new(Some(child)));

    while let Some(command) = rx.recv().await {
        match command {
            WorkerCommand::NewSession { respond_to } => {
                let conn = Rc::clone(&conn);
                let sessions = sessions.clone();
                let server_name = name.clone();
                tokio::task::spawn_local(async move {
                    let result = async {
                        let response = conn
                            .new_session(acp::NewSessionRequest::new(std::env::current_dir()?))
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
                });
            }
            WorkerCommand::Prompt {
                session_id,
                message,
                respond_to,
            } => {
                let conn = Rc::clone(&conn);
                let sessions = sessions.clone();
                let server_name = name.clone();
                tokio::task::spawn_local(async move {
                    let result = async {
                        {
                            let mut sessions = sessions.write().await;
                            let state = sessions.entry(session_id.clone()).or_default();
                            state.response_text.clear();
                            state.stop_reason = None;
                        }

                        let response = conn
                            .prompt(acp::PromptRequest::new(
                                session_id.clone(),
                                vec![message.into()],
                            ))
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
                });
            }
            WorkerCommand::LoadSession {
                session_id,
                respond_to,
            } => {
                let conn = Rc::clone(&conn);
                let sessions = sessions.clone();
                let server_name = name.clone();
                tokio::task::spawn_local(async move {
                    let result = async {
                        conn.load_session(acp::LoadSessionRequest::new(
                            session_id.clone(),
                            std::env::current_dir()?,
                        ))
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
                });
            }
            WorkerCommand::CancelSession {
                session_id,
                respond_to,
            } => {
                let conn = Rc::clone(&conn);
                let server_name = name.clone();
                tokio::task::spawn_local(async move {
                    let result = conn
                        .cancel(acp::CancelNotification::new(session_id.clone()))
                        .await
                        .with_context(|| {
                            format!(
                                "Failed to cancel ACP session '{}' on '{}'",
                                session_id, server_name
                            )
                        });
                    let _ = respond_to.send(result);
                });
            }
            WorkerCommand::Shutdown { respond_to } => {
                let result = shutdown_child(&child).await;
                let _ = respond_to.send(result);
                break;
            }
        }
    }

    Ok(())
}

async fn shutdown_child(child: &Rc<RefCell<Option<Child>>>) -> Result<()> {
    let child = child.borrow_mut().take();
    if let Some(mut child) = child {
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

fn resolve_notification_source(
    fallback_agent: &str,
    notification: &acp::SessionNotification,
) -> AgentSource {
    let session_id = notification.session_id.0.to_string();
    let (update_agent, update_session) = match &notification.update {
        acp::SessionUpdate::AgentMessageChunk(chunk) => {
            let meta = chunk.meta.as_ref().map(|meta| json!(meta));
            (
                meta.as_ref().and_then(agent_from_meta_value),
                meta.as_ref().and_then(session_from_meta_value),
            )
        }
        acp::SessionUpdate::AgentThoughtChunk(chunk) => {
            let meta = chunk.meta.as_ref().map(|meta| json!(meta));
            (
                meta.as_ref().and_then(agent_from_meta_value),
                meta.as_ref().and_then(session_from_meta_value),
            )
        }
        acp::SessionUpdate::ToolCall(call) => {
            let meta = call.meta.as_ref().map(|meta| json!(meta));
            (
                meta.as_ref().and_then(agent_from_meta_value),
                meta.as_ref().and_then(session_from_meta_value),
            )
        }
        acp::SessionUpdate::ToolCallUpdate(update) => {
            let meta = update.meta.as_ref().map(|meta| json!(meta));
            (
                meta.as_ref().and_then(agent_from_meta_value),
                meta.as_ref().and_then(session_from_meta_value),
            )
        }
        acp::SessionUpdate::Plan(plan) => {
            let meta = plan.meta.as_ref().map(|meta| json!(meta));
            (
                meta.as_ref().and_then(agent_from_meta_value),
                meta.as_ref().and_then(session_from_meta_value),
            )
        }
        acp::SessionUpdate::SessionInfoUpdate(info) => {
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
            )
        }
        _ => (None, None),
    };

    AgentSource {
        agent: update_agent.unwrap_or_else(|| fallback_agent.to_string()),
        session_id: Some(update_session.unwrap_or(session_id)),
    }
}

fn session_info_update_event(
    info: &acp::SessionInfoUpdate,
    source: AgentSource,
) -> Option<AgentEvent> {
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

/// Parse an ACP `status` string into the typed `ToolStatus`. Accepts the
/// snake_case form emitted by the ACP SDK as well as the PascalCase /
/// kebab-case forms that upstream producers sometimes use.
fn parse_tool_status_str(status: &str) -> Option<ToolStatus> {
    match status {
        "pending" | "Pending" => Some(ToolStatus::Pending),
        "in_progress" | "InProgress" | "in-progress" => Some(ToolStatus::InProgress),
        "completed" | "Completed" => Some(ToolStatus::Completed),
        "failed" | "Failed" => Some(ToolStatus::Failed),
        _ => None,
    }
}

/// Format an `AgentSource` as a single-line session heading, matching the
/// rendering used by the harnx TUI. Kept inline here so this crate has no
/// reverse dependency on harnx rendering helpers.
fn source_heading(source: &AgentSource) -> String {
    match &source.session_id {
        Some(session_id) if !session_id.is_empty() => {
            format!("> {} ▸ {}", source.agent, session_id)
        }
        _ => format!("> {}", source.agent),
    }
}

fn chunk_text(content: &acp::ContentBlock) -> String {
    match content {
        acp::ContentBlock::Text(text) => text.text.clone(),
        acp::ContentBlock::ResourceLink(link) => link.uri.to_string(),
        acp::ContentBlock::Image(_) => "<image>".to_string(),
        acp::ContentBlock::Audio(_) => "<audio>".to_string(),
        acp::ContentBlock::Resource(_) => "<resource>".to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::Client as _;
    use serde_json::json;

    #[test]
    fn session_info_update_event_emits_usage_model_event() {
        let info = acp::SessionInfoUpdate::new().meta(serde_json::Map::from_iter([(
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
        let notification = acp::SessionNotification::new(
            acp::SessionId::new("outer-session"),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new("hello".into())),
        );

        let source = resolve_notification_source("argus", &notification);
        assert_eq!(source.agent, "argus");
        assert_eq!(source.session_id.as_deref(), Some("outer-session"));
    }

    #[test]
    fn resolve_notification_source_uses_nested_session_when_present() {
        let notification = acp::SessionNotification::new(
            acp::SessionId::new("outer-session"),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new("hello".into()).meta(
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
        let notification = acp::SessionNotification::new(
            acp::SessionId::new("outer-session"),
            acp::SessionUpdate::ToolCall(acp::ToolCall::new("ls", "path: /tmp").meta(
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
        // Unit test: inject a stale (dead) worker handle directly into the
        // client and verify that `ensure_connected()` detects the dead worker,
        // resets state, and then attempts to reconnect (the reconnect will fail
        // because the command doesn't exist, but the important thing is that
        // `connected` is reset to false rather than being left stuck at true).

        let config = AcpServerConfig {
            name: "mock-dead".to_string(),
            command: "__harnx_test_nonexistent_binary__".to_string(),
            args: vec![],
            env: HashMap::new(),
            enabled: true,
            description: None,
            idle_timeout_secs: 5,
            operation_timeout_secs: 30,
        };
        let client = AcpClient::new(config);

        // Create a dead worker handle: drop dead_tx immediately so dead_rx
        // will return Err(TryRecvError::Closed) on try_recv(), which is the
        // death signal.  (Dropping without sending yields Closed, not Ok(()).)
        let (tx, _rx) = mpsc::unbounded_channel::<WorkerCommand>();
        let (abort_tx, _abort_rx) = oneshot::channel::<()>();
        let (dead_tx, dead_rx) = oneshot::channel::<()>();
        drop(dead_tx); // sender dropped → dead_rx fires immediately

        // Build a fake join handle by spawning a thread that does nothing.
        let join = thread::spawn(|| {});

        let stale_handle = AcpWorkerHandle {
            tx,
            join,
            abort_tx,
            dead_rx,
        };

        // Plant the stale handle and set connected = true to simulate the
        // post-crash stuck state.
        *client.worker.lock().await = Some(stale_handle);
        *client.connected.write().await = true;

        // ensure_connected() must detect the dead worker and attempt to
        // reconnect.  The reconnect fails (no such binary), so it returns Err.
        let result = client.ensure_connected().await;

        // The critical assertion: connected must be false, not stuck at true.
        assert!(
            !*client.connected.read().await,
            "connected must be reset after dead worker detected"
        );
        // All connection state must be cleared, not just `connected`.
        assert!(
            client.sessions.read().await.is_empty(),
            "sessions must be cleared after dead worker detected"
        );
        assert!(
            client.initialize_response.read().await.is_none(),
            "initialize_response must be cleared after dead worker detected"
        );
        // Worker handle must be gone too.
        assert!(
            client.worker.lock().await.is_none(),
            "worker must be cleared after dead worker detected"
        );
        // The reconnect error proves a fresh spawn was attempted.
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

        let notification = acp::SessionNotification::new(
            acp::SessionId::new("outer-session"),
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new("call-1", "pytheas_session_prompt")
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

        match &forwarded_event {
            AgentEvent::Tool(ToolEvent::Started { name, input, .. }) => {
                assert_eq!(name, "pytheas_session_prompt");
                assert_eq!(
                    input["message"],
                    json!("Count files in /tmp using ls first.")
                );
                assert_eq!(
                    input["session_id"],
                    json!("608e48b6-c880-4168-b028-1bda3469be07")
                );
            }
            other => panic!("unexpected forwarded event: {other:?}"),
        }
        let source = forwarded_source.expect("forwarded agent source");
        assert_eq!(source.agent, "pytheas");
        assert_eq!(
            source.session_id.as_deref(),
            Some("608e48b6-c880-4168-b028-1bda3469be07")
        );
    }
}
