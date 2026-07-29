use crate::{compat::TokioCompat, AcpServerConfig, NestedAcpEvent};

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock as AcpContentBlock, CreateTerminalRequest,
    CreateTerminalResponse, Implementation, InitializeRequest, InitializeResponse,
    KillTerminalRequest, KillTerminalResponse, LoadSessionRequest, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, ReadTextFileRequest, ReadTextFileResponse,
    RequestPermissionRequest, RequestPermissionResponse, SessionInfoUpdate, SessionNotification,
    SessionUpdate, WaitForTerminalExitRequest, WaitForTerminalExitResponse, WriteTextFileRequest,
    WriteTextFileResponse,
};
use agent_client_protocol::schema::ProtocolVersion;
use anyhow::{anyhow, bail, Context, Result};
use harnx_core::event::{
    AgentEvent, AgentSource, ContentBlock, ModelEvent, PlanEntry, ToolEvent, ToolKind, ToolStatus,
    UserEvent,
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

/// Session-id sentinel used for generic subprocess liveness signals that are
/// not tied to a specific session (inbound requests, stderr output). Real ACP
/// session IDs are never empty, so the empty string unambiguously marks a
/// "still alive" heartbeat that resets the `session_prompt` idle timer
/// regardless of which session is active (see issue #874).
const LIVENESS_SENTINEL: &str = "";

/// Decide whether an `activity_rx` recv result should reset the prompt idle
/// timer.
///
/// Returns `true` when the event proves the subprocess is still alive:
/// - an `Ok` whose session id matches the active prompt, OR is the generic
///   liveness sentinel (empty string) emitted by inbound requests / stderr;
/// - a `Lagged` error, since broadcast overflow means we missed liveness
///   messages under heavy activity — itself proof of liveness.
///
/// Returns `false` for unrelated session ids and for a `Closed` channel.
fn idle_activity_resets_timer(
    result: &Result<String, broadcast::error::RecvError>,
    session_id: &str,
) -> bool {
    match result {
        Ok(sid) => sid == session_id || sid == LIVENESS_SENTINEL,
        Err(broadcast::error::RecvError::Lagged(_)) => true,
        Err(broadcast::error::RecvError::Closed) => false,
    }
}

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
        let event = AgentEvent::sub_agent(source, event);
        let mut forwarders = self.chunk_forwarder.write().await;
        let mut forwarded_to_chunk = false;
        forwarders.retain(
            |_, tx| match tx.send(NestedAcpEvent::Agent(event.clone())) {
                Ok(()) => {
                    forwarded_to_chunk = true;
                    true
                }
                Err(_) => false,
            },
        );

        if !forwarded_to_chunk {
            harnx_core::sink::emit_agent_event(event);
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

        let mut accumulate_response_text = false;
        let mut message_text: Option<String> = None;
        let event: Option<AgentEvent> = match args.update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                let text = chunk_text(&chunk.content);
                let meta = chunk.meta.as_ref().map(|meta| json!(meta));
                let is_error = meta.as_ref().is_some_and(is_error_from_meta_value);
                // Only drop genuinely empty chunks. Whitespace-only chunks
                // (e.g. a lone "\n" between streamed list items or paragraphs)
                // are meaningful: stripping them concatenates adjacent chunks
                // and corrupts rendered markdown (see issue #862, where
                // "1. Confirm\n2. Confirm" rendered as "1. Confirm2. Confirm").
                if text.is_empty() {
                    None
                } else if is_error {
                    Some(AgentEvent::Model(ModelEvent::Error(
                        strip_error_prefix(&text).to_string(),
                    )))
                } else {
                    accumulate_response_text = true;
                    message_text = Some(text.clone());
                    Some(AgentEvent::Model(ModelEvent::MessageChunk {
                        blocks: vec![ContentBlock::Text(text)],
                    }))
                }
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                let text = chunk_text(&chunk.content);
                // See AgentMessageChunk above: preserve whitespace-only chunks
                // so streamed thoughts keep their newlines/spacing.
                if text.is_empty() {
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
            SessionUpdate::UserMessageChunk(chunk) => {
                // User turns (e.g. from remote attach/resume history replay)
                // are rendered as user transcript entries and deliberately
                // NOT accumulated into `response_text` (`accumulate_response_text`
                // is only set for non-error `AgentMessageChunk`s).
                let text = chunk_text(&chunk.content);
                if text.is_empty() {
                    None
                } else {
                    Some(AgentEvent::User(UserEvent::Message { content: text }))
                }
            }
            SessionUpdate::AvailableCommandsUpdate(_)
            | SessionUpdate::CurrentModeUpdate(_)
            | SessionUpdate::ConfigOptionUpdate(_) => None,
            other => {
                log::debug!("Unhandled ACP SessionUpdate variant: {:?}", other);
                None
            }
        };

        if let Some(event) = event {
            if accumulate_response_text {
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

    /// Emit a generic liveness signal that is not tied to a specific session.
    ///
    /// Any inbound traffic from the subprocess (requests, stderr output) proves
    /// the remote agent is still alive even when it is not streaming
    /// `session/update` notifications for the active prompt. We tag these with
    /// an empty session id sentinel so the `session_prompt` idle timer can
    /// reset on them regardless of which session is currently active (see
    /// issue #874).
    fn signal_liveness(&self) {
        let _ = self.activity_tx.send(LIVENESS_SENTINEL.to_string());
    }

    async fn request_permission(
        &self,
        _args: RequestPermissionRequest,
    ) -> acp::Result<RequestPermissionResponse> {
        self.signal_liveness();
        Err(acp::Error::method_not_found())
    }

    async fn write_text_file(
        &self,
        _args: WriteTextFileRequest,
    ) -> acp::Result<WriteTextFileResponse> {
        self.signal_liveness();
        Err(acp::Error::method_not_found())
    }

    async fn read_text_file(
        &self,
        _args: ReadTextFileRequest,
    ) -> acp::Result<ReadTextFileResponse> {
        self.signal_liveness();
        Err(acp::Error::method_not_found())
    }

    async fn create_terminal(
        &self,
        _args: CreateTerminalRequest,
    ) -> acp::Result<CreateTerminalResponse> {
        self.signal_liveness();
        Err(acp::Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _args: WaitForTerminalExitRequest,
    ) -> acp::Result<WaitForTerminalExitResponse> {
        self.signal_liveness();
        Err(acp::Error::method_not_found())
    }

    async fn kill_terminal(&self, _args: KillTerminalRequest) -> acp::Result<KillTerminalResponse> {
        self.signal_liveness();
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

    pub fn description(&self) -> Option<&str> {
        self.config.description.as_deref()
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
                    // Best-effort cancel so the subprocess stops abandoned work
                    // (issue #874). Fire-and-forget to avoid adding latency.
                    self.request_session_cancel_best_effort(&session_id).await;
                    bail!("ACP server '{}' timed out during session/prompt (overall timeout)", self.name);
                }
                _ = &mut idle_timeout => {
                    // The remote agent may still be working (long, quiet tool
                    // runs can exceed the idle window). Proactively cancel the
                    // session so the subprocess stops abandoned work instead of
                    // producing a result nobody consumes (issue #874). Cancel is
                    // fire-and-forget so it adds no latency to surfacing the error.
                    self.request_session_cancel_best_effort(&session_id).await;
                    bail!(
                        "ACP server '{}' timed out during session/prompt (idle timeout); \
                         the remote agent may still be running",
                        self.name
                    );
                }
                result = activity_rx.recv() => {
                    // Reset the idle timer on any liveness signal: an update for
                    // this session, a generic liveness sentinel from inbound
                    // requests / subprocess stderr, or broadcast overflow
                    // (Lagged). See `idle_activity_resets_timer` (issue #874).
                    if idle_activity_resets_timer(&result, &session_id) {
                        idle_timeout.as_mut().reset(tokio::time::Instant::now() + self.idle_timeout);
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

    /// Fire-and-forget `session/cancel`: enqueue a cancel command for the
    /// session without awaiting its response.
    ///
    /// Used by the `session_prompt` timeout branches (issue #874) so that
    /// cancelling abandoned remote work does not add a second timeout's worth of
    /// latency before the timeout error is surfaced to the caller. Best-effort:
    /// failures to enqueue are ignored because we are already returning an
    /// error.
    async fn request_session_cancel_best_effort(&self, session_id: &str) {
        let Ok(tx) = self.worker_sender().await else {
            return;
        };
        let (respond_to, _response_rx) = oneshot::channel();
        let _ = tx.send(WorkerCommand::CancelSession {
            session_id: session_id.to_owned(),
            respond_to,
        });
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
        // Every ACP server spawned by AcpClient is a worker-owned tool or
        // sub-agent backend. Force the internal role after configured env so
        // user config cannot accidentally turn it back into a NATS frontend.
        .env(crate::ACP_EXECUTION_ROLE_ENV, crate::ACP_BACKEND_ROLE)
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
        // Subprocess stderr output is a liveness signal: the remote agent is
        // still running even if it is not streaming `session/update`
        // notifications. Reset the prompt idle timer on it (issue #874).
        //
        // Note: this is line-buffered, so a subprocess that writes stderr bytes
        // without a trailing newline for longer than the idle window will not
        // emit a liveness signal from those bytes. Inbound ACP requests and
        // `session/update` notifications remain the primary liveness sources;
        // stderr is a best-effort supplement. Accepted residual risk.
        let stderr_activity_tx = activity_tx.clone();
        tokio::task::spawn_local(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = stderr_activity_tx.send(LIVENESS_SENTINEL.to_string());
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

fn is_error_from_meta_value(value: &serde_json::Value) -> bool {
    value
        .get("harnx:error")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn strip_error_prefix(text: &str) -> &str {
    text.strip_prefix("error: ").unwrap_or(text)
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
    use agent_client_protocol::schema::v1::{
        ContentChunk, SessionId, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    };
    use serde_json::json;

    fn unwrap_sub_agent_event(event: NestedAcpEvent) -> NestedAcpEvent {
        match event {
            NestedAcpEvent::Agent(AgentEvent::SubAgent { event, .. }) => {
                NestedAcpEvent::Agent(*event)
            }
            event => event,
        }
    }

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

        let forwarded_source = match &forwarded {
            NestedAcpEvent::Agent(AgentEvent::SubAgent { source, .. }) => Some(source.clone()),
            _ => None,
        };
        let forwarded = unwrap_sub_agent_event(forwarded);
        let forwarded_event = match forwarded {
            NestedAcpEvent::Agent(event) => event,
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

        let forwarded_source = match &forwarded {
            NestedAcpEvent::Agent(AgentEvent::SubAgent { source, .. }) => Some(source.clone()),
            _ => None,
        };
        let forwarded = unwrap_sub_agent_event(forwarded);
        let forwarded_event = match forwarded {
            NestedAcpEvent::Agent(event) => event,
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

        let forwarded_source = match &forwarded {
            NestedAcpEvent::Agent(AgentEvent::SubAgent { source, .. }) => Some(source.clone()),
            _ => None,
        };
        let forwarded = unwrap_sub_agent_event(forwarded);
        let forwarded_event = match forwarded {
            NestedAcpEvent::Agent(event) => event,
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

        let forwarded = chunk_rx.recv().await.expect("forwarded nested ACP event");
        let NestedAcpEvent::Agent(forwarded_event) = unwrap_sub_agent_event(forwarded) else {
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

        let forwarded = unwrap_sub_agent_event(forwarded);
        let forwarded_event = match forwarded {
            NestedAcpEvent::Agent(event) => event,
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

    /// Regression test for issue #862: whitespace-only message chunks (e.g. a
    /// lone "\n" streamed between list items) must NOT be dropped. Dropping them
    /// concatenated adjacent chunks, rendering "1. Confirm\n2. Confirm" as
    /// "1. Confirm2. Confirm" in the TUI.
    #[tokio::test]
    async fn whitespace_only_message_chunk_is_forwarded_not_stripped() {
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

        // Stream chunks the way an agent emits a numbered list: text, then
        // standalone whitespace separators (newline, spaces, mixed), then more
        // text. Every whitespace-only chunk must survive so spacing is intact.
        let pieces = ["1. Confirm", "\n", "2. Confirm", " ", "(tab\t)", "\n \t "];
        for piece in pieces {
            let notification = SessionNotification::new(
                SessionId::new("outer-session"),
                SessionUpdate::AgentMessageChunk(ContentChunk::new(piece.into())),
            );
            client.session_notification(notification).await.unwrap();
        }

        let mut received = String::new();
        for _ in 0..pieces.len() {
            let forwarded = chunk_rx.recv().await.expect("forwarded chunk");

            let forwarded = unwrap_sub_agent_event(forwarded);
            match forwarded {
                NestedAcpEvent::Agent(AgentEvent::Model(ModelEvent::MessageChunk { blocks })) => {
                    for b in &blocks {
                        if let ContentBlock::Text(t) = b {
                            received.push_str(t);
                        }
                    }
                }
                other => panic!("unexpected nested ACP event: {other:?}"),
            }
        }

        // All whitespace (newline, spaces, tab) must be preserved verbatim so
        // markdown renders the two list items on separate lines.
        let expected = "1. Confirm\n2. Confirm (tab\t)\n \t ";
        assert_eq!(received, expected);

        // The accumulated response_text must also retain the whitespace.
        let sessions = sessions.read().await;
        let state = sessions
            .get("outer-session")
            .expect("session state recorded");
        assert_eq!(state.response_text, expected);
    }

    /// User-message chunks (e.g. from remote attach/resume history replay) must
    /// be forwarded as `AgentEvent::User` for rendering, but must NOT be
    /// accumulated into `response_text` (which forms the next agent's input).
    #[tokio::test]
    async fn user_message_chunk_forwards_user_event_without_polluting_response_text() {
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
            SessionUpdate::UserMessageChunk(ContentChunk::new("hello from attach".into())),
        );
        client.session_notification(notification).await.unwrap();

        let forwarded = chunk_rx.recv().await.expect("forwarded user chunk");

        let forwarded = unwrap_sub_agent_event(forwarded);
        match forwarded {
            NestedAcpEvent::Agent(AgentEvent::User(UserEvent::Message { content })) => {
                assert_eq!(content, "hello from attach");
            }
            other => panic!("unexpected nested ACP event: {other:?}"),
        }

        // Critically: a user turn must NOT be appended to response_text.
        let sessions = sessions.read().await;
        if let Some(state) = sessions.get("outer-session") {
            assert!(
                state.response_text.is_empty(),
                "user message chunk must not accumulate into response_text"
            );
        }
    }

    /// Regression test for issue #862, thought-chunk path: whitespace-only
    /// `AgentThoughtChunk`s (e.g. a lone "\n" between streamed thought lines)
    /// must NOT be dropped, otherwise thoughts concatenate the same way.
    #[tokio::test]
    async fn whitespace_only_thought_chunk_is_forwarded_not_stripped() {
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

        for piece in ["Step one", "\n", "Step two"] {
            let notification = SessionNotification::new(
                SessionId::new("outer-session"),
                SessionUpdate::AgentThoughtChunk(ContentChunk::new(piece.into())),
            );
            client.session_notification(notification).await.unwrap();
        }

        let mut received = String::new();
        for _ in 0..3 {
            let forwarded = chunk_rx.recv().await.expect("forwarded thought chunk");

            let forwarded = unwrap_sub_agent_event(forwarded);
            match forwarded {
                NestedAcpEvent::Agent(AgentEvent::Model(ModelEvent::ThoughtChunk { blocks })) => {
                    for b in &blocks {
                        if let ContentBlock::Text(t) = b {
                            received.push_str(t);
                        }
                    }
                }
                other => panic!("unexpected nested ACP event: {other:?}"),
            }
        }

        assert_eq!(received, "Step one\nStep two");
    }

    /// Error chunks from nested ACP agents must surface as `ModelEvent::Error`
    /// without being accumulated into `response_text`, which becomes next
    /// agent input.
    #[tokio::test]
    async fn error_message_chunk_forwards_error_event_without_polluting_response_text() {
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
            SessionUpdate::AgentMessageChunk(
                ContentChunk::new("error: upstream failed".into()).meta(
                    serde_json::Map::from_iter([("harnx:error".to_string(), json!(true))]),
                ),
            ),
        );
        client.session_notification(notification).await.unwrap();

        let forwarded = chunk_rx.recv().await.expect("forwarded error chunk");

        let forwarded = unwrap_sub_agent_event(forwarded);
        match forwarded {
            NestedAcpEvent::Agent(AgentEvent::Model(ModelEvent::Error(message))) => {
                assert_eq!(message, "upstream failed");
            }
            other => panic!("unexpected nested ACP event: {other:?}"),
        }

        let sessions = sessions.read().await;
        if let Some(state) = sessions.get("outer-session") {
            assert!(
                state.response_text.is_empty(),
                "error message chunk must not accumulate into response_text"
            );
        }
    }

    /// A chunk with `harnx:error = false` is a normal message chunk: it must
    /// forward a `MessageChunk` event and DO accumulate into `response_text`.
    #[tokio::test]
    async fn message_chunk_with_error_meta_false_is_treated_as_normal_message() {
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
            SessionUpdate::AgentMessageChunk(ContentChunk::new("hello".into()).meta(
                serde_json::Map::from_iter([("harnx:error".to_string(), json!(false))]),
            )),
        );
        client.session_notification(notification).await.unwrap();

        let forwarded = chunk_rx.recv().await.expect("forwarded message chunk");

        let forwarded = unwrap_sub_agent_event(forwarded);
        match forwarded {
            NestedAcpEvent::Agent(AgentEvent::Model(ModelEvent::MessageChunk { .. })) => {}
            other => panic!("unexpected nested ACP event: {other:?}"),
        }

        let sessions = sessions.read().await;
        let state = sessions.get("outer-session").expect("session state");
        assert_eq!(
            state.response_text, "hello",
            "non-error message chunk must accumulate into response_text"
        );
    }

    /// A `harnx:error` meta with a non-boolean value falls back to normal
    /// message handling (fails open safely, not treated as an error).
    #[tokio::test]
    async fn message_chunk_with_non_bool_error_meta_is_treated_as_normal_message() {
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
            SessionUpdate::AgentMessageChunk(ContentChunk::new("hi".into()).meta(
                serde_json::Map::from_iter([("harnx:error".to_string(), json!("yes"))]),
            )),
        );
        client.session_notification(notification).await.unwrap();

        let forwarded = chunk_rx.recv().await.expect("forwarded message chunk");

        let forwarded = unwrap_sub_agent_event(forwarded);
        match forwarded {
            NestedAcpEvent::Agent(AgentEvent::Model(ModelEvent::MessageChunk { .. })) => {}
            other => panic!("unexpected nested ACP event: {other:?}"),
        }

        let sessions = sessions.read().await;
        let state = sessions.get("outer-session").expect("session state");
        assert_eq!(state.response_text, "hi");
    }

    /// An error-flagged chunk WITHOUT the legacy `error: ` prefix still surfaces
    /// as a `ModelEvent::Error` carrying the raw text, and is not accumulated.
    #[tokio::test]
    async fn error_chunk_without_prefix_forwards_raw_error_text() {
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
            SessionUpdate::AgentMessageChunk(ContentChunk::new("bare failure".into()).meta(
                serde_json::Map::from_iter([("harnx:error".to_string(), json!(true))]),
            )),
        );
        client.session_notification(notification).await.unwrap();

        let forwarded = chunk_rx.recv().await.expect("forwarded error chunk");

        let forwarded = unwrap_sub_agent_event(forwarded);
        match forwarded {
            NestedAcpEvent::Agent(AgentEvent::Model(ModelEvent::Error(message))) => {
                assert_eq!(message, "bare failure");
            }
            other => panic!("unexpected nested ACP event: {other:?}"),
        }

        let sessions = sessions.read().await;
        if let Some(state) = sessions.get("outer-session") {
            assert!(state.response_text.is_empty());
        }
    }

    /// A genuinely empty thought chunk ("") is still dropped.
    #[tokio::test]
    async fn empty_thought_chunk_is_dropped() {
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
            SessionUpdate::AgentThoughtChunk(ContentChunk::new("".into())),
        );
        client.session_notification(notification).await.unwrap();

        assert!(
            chunk_rx.try_recv().is_err(),
            "empty thought chunk should not be forwarded"
        );
    }

    /// A genuinely empty chunk ("") carries no content and is still dropped.
    #[tokio::test]
    async fn empty_message_chunk_is_dropped() {
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
            SessionUpdate::AgentMessageChunk(ContentChunk::new("".into())),
        );
        client.session_notification(notification).await.unwrap();

        assert!(
            chunk_rx.try_recv().is_err(),
            "empty chunk should not be forwarded"
        );
    }

    /// Generic liveness signals (emitted by inbound requests and subprocess
    /// stderr) are tagged with an empty session id sentinel so the
    /// `session_prompt` idle timer resets on them regardless of which session
    /// is active. Regression test for issue #874.
    #[tokio::test]
    async fn signal_liveness_emits_empty_session_id_sentinel() {
        let sessions = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let chunk_forwarder = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let (activity_tx, mut activity_rx) = tokio::sync::broadcast::channel(8);
        let client = AcpNotificationClient::new(
            "working".to_string(),
            sessions,
            chunk_forwarder,
            activity_tx,
        );

        client.signal_liveness();

        let sid = activity_rx
            .try_recv()
            .expect("liveness signal should be broadcast");
        assert_eq!(
            sid, "",
            "generic liveness must use the empty session id sentinel so the idle timer resets regardless of session"
        );
    }

    /// A `session/update` notification still emits an activity signal tagged
    /// with its own session id, so the idle timer resets for the matching
    /// session (existing behavior, kept alongside the #874 generic signal).
    #[tokio::test]
    async fn session_notification_emits_matching_session_id_activity() {
        let sessions = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let chunk_forwarder = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let (activity_tx, mut activity_rx) = tokio::sync::broadcast::channel(8);
        let client = AcpNotificationClient::new(
            "working".to_string(),
            sessions,
            chunk_forwarder,
            activity_tx,
        );

        let notification = SessionNotification::new(
            SessionId::new("outer-session"),
            SessionUpdate::AgentMessageChunk(ContentChunk::new("hi".into())),
        );
        client.session_notification(notification).await.unwrap();

        let sid = activity_rx
            .try_recv()
            .expect("session notification should broadcast activity");
        assert_eq!(
            sid, "outer-session",
            "session notifications must be tagged with their session id"
        );
    }

    // ---- session_prompt idle-timer decision logic (issue #874) ----

    #[test]
    fn idle_activity_resets_on_matching_session_id() {
        let result: Result<String, broadcast::error::RecvError> = Ok("sess-1".to_string());
        assert!(
            idle_activity_resets_timer(&result, "sess-1"),
            "activity for the active session must reset the idle timer"
        );
    }

    #[test]
    fn idle_activity_resets_on_generic_liveness_sentinel() {
        // Empty-string sentinel = generic liveness from inbound requests/stderr.
        let result: Result<String, broadcast::error::RecvError> = Ok(LIVENESS_SENTINEL.to_string());
        assert!(
            idle_activity_resets_timer(&result, "sess-1"),
            "generic liveness sentinel must reset the idle timer regardless of active session"
        );
    }

    #[test]
    fn idle_activity_ignores_unrelated_session_id() {
        let result: Result<String, broadcast::error::RecvError> = Ok("other-session".to_string());
        assert!(
            !idle_activity_resets_timer(&result, "sess-1"),
            "a non-empty, non-matching session id must NOT reset the idle timer"
        );
    }

    #[test]
    fn idle_activity_resets_on_lagged() {
        // Broadcast overflow under heavy activity is itself proof of liveness.
        let result: Result<String, broadcast::error::RecvError> =
            Err(broadcast::error::RecvError::Lagged(7));
        assert!(
            idle_activity_resets_timer(&result, "sess-1"),
            "Lagged (broadcast overflow) must reset the idle timer"
        );
    }

    #[test]
    fn idle_activity_does_not_reset_on_closed() {
        let result: Result<String, broadcast::error::RecvError> =
            Err(broadcast::error::RecvError::Closed);
        assert!(
            !idle_activity_resets_timer(&result, "sess-1"),
            "a closed activity channel must NOT reset the idle timer"
        );
    }

    /// On idle timeout, `session_prompt` must send `session/cancel` for the
    /// active session before returning the (softened) idle-timeout error, so the
    /// remote subprocess stops abandoned work (issue #874). A short 1s idle
    /// timeout (well under the 30s overall timeout) keeps the test fast while
    /// still exercising the real timeout branch.
    #[tokio::test]
    async fn session_prompt_cancels_session_on_idle_timeout() {
        let config = AcpServerConfig {
            name: "mock-idle".to_string(),
            command: "__harnx_test_nonexistent_binary__".to_string(),
            args: vec![],
            env: HashMap::new(),
            enabled: true,
            description: None,
            idle_timeout_secs: 1,
            operation_timeout_secs: 30,
            package: None,
        };
        let client = AcpClient::new(config);

        // Inject a live worker handle so `connect()` short-circuits to Ok and
        // `session_prompt` proceeds into its select loop. Keep `dead_tx` alive
        // so `dead_rx.try_recv()` reports Empty (worker considered alive).
        let (tx, mut rx) = mpsc::unbounded_channel::<WorkerCommand>();
        let (abort_tx, _abort_rx) = oneshot::channel::<()>();
        let (dead_tx, dead_rx) = oneshot::channel::<()>();
        let join = thread::spawn(|| {});
        *client.worker.lock().await = Some(AcpWorkerHandle {
            tx,
            join,
            abort_tx,
            dead_rx,
        });
        *client.connected.write().await = true;

        // The worker rx is never serviced, so no response ever arrives: the
        // idle timer fires first. session_cancel's own response also never
        // arrives, but its timeout is ignored by session_prompt.
        let result = client.session_prompt(Some("sess-cancel"), "hello").await;

        let err = result.expect_err("session_prompt must error on idle timeout");
        let msg = err.to_string();
        assert!(
            msg.contains("idle timeout"),
            "expected idle-timeout error, got: {msg}"
        );
        assert!(
            msg.contains("may still be running"),
            "idle-timeout error should hint the remote may still be running, got: {msg}"
        );

        // Drain the worker command queue: it must contain the initial Prompt and
        // a CancelSession for the active session id.
        let mut saw_prompt = false;
        let mut saw_cancel_for_session = false;
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                WorkerCommand::Prompt { session_id, .. } => {
                    assert_eq!(session_id, "sess-cancel");
                    saw_prompt = true;
                }
                WorkerCommand::CancelSession { session_id, .. } if session_id == "sess-cancel" => {
                    saw_cancel_for_session = true;
                }
                _ => {}
            }
        }
        assert!(saw_prompt, "expected an initial Prompt command");
        assert!(
            saw_cancel_for_session,
            "expected a CancelSession command for the active session on idle timeout"
        );

        drop(dead_tx);
    }
}
