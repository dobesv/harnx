//! harnx-acp-server — ACP server front-end (HarnxAgent) extracted from
//! `harnx::acp::server` (plan P48, β+ progressive peel). Binds the ACP
//! protocol (from `harnx-acp`) to harnx-runtime's Config/Input/Client/tool
//! types.

#[macro_use]
extern crate log;

mod server_main;

pub use server_main::run;

#[cfg(test)]
mod lib_tests;
#[cfg(test)]
mod test_regression_issue_68;

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use harnx_core::event::{AgentEvent, AgentSource, ModelEvent, NoticeEvent, ToolEvent, UserEvent};
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::utils::{AbortSignal, AbortSignalInner};

/// Idle session reaper TTL: evict SessionContexts idle > 15 minutes.
const SESSION_IDLE_TTL: Duration = Duration::from_secs(15 * 60);

/// Update payloads forwarded from the per-prompt `AcpChunkSink` to the
/// local `fwd_task`. Each variant carries the original `AgentSource` so
/// the forwarded `SessionNotification` can attach `meta` describing
/// which agent (parent vs. some sub-agent) actually produced the event;
/// the parent's `AcpNotificationClient::resolve_notification_source`
/// reads that meta to render the right `> agent ▸ session` heading.
#[derive(Debug)]
enum AcpForward {
    /// Text chunk for `SessionUpdate::AgentMessageChunk`.
    Text(String, Option<AgentSource>),
    /// User-turn text for `SessionUpdate::UserMessageChunk`. Kept separate
    /// from `Text` so replayed/attached user turns are NOT mixed into the
    /// parent's accumulated `response_text` (which forms the next agent's
    /// input). The parent client renders these as user transcript entries.
    UserText(String, Option<AgentSource>),
    /// Sub-agent tool invocation for `SessionUpdate::ToolCall`.
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
        markdown: Option<String>,
        source: Option<AgentSource>,
    },
    /// Sub-agent tool status/progress update for `SessionUpdate::ToolCallUpdate`.
    ToolUpdate {
        id: String,
        markdown: Option<String>,
        status: Option<harnx_core::event::ToolStatus>,
        source: Option<AgentSource>,
    },
    /// Sub-agent tool completion for `SessionUpdate::ToolCallUpdate` with status=completed.
    ToolCompleted {
        id: String,
        output: serde_json::Value,
        markdown: Option<String>,
        source: Option<AgentSource>,
    },
}

/// An `AgentEventSink` installed during each ACP prompt turn.
/// Forwards events from unified `run_agent_loop` through a channel to a single
/// draining task that emits ACP `session_notification`s in order. The channel
/// keeps the sink cheap and decouples event production from the notification
/// send path. The draining task emits each notification synchronously (inline,
/// not on a separate spawned task) so that wire order matches production order:
/// `send_notification` only enqueues onto the connection's outgoing stream, so
/// spawning one task per chunk let a multi-thread runtime reorder adjacent
/// chunks, scrambling the parent's reconstructed message.
struct AcpChunkSink {
    tx: tokio::sync::mpsc::UnboundedSender<AcpForward>,
}

impl harnx_core::event::AgentEventSink for AcpChunkSink {
    fn emit(&self, event: AgentEvent, source: Option<AgentSource>) {
        // Source headings are NOT injected into the chunk stream here.
        // Sending `> agent ▸ session` as an `AgentMessageChunk` would
        // pollute the parent's accumulated `response_text` (which forms
        // the next agent's input, see `AcpNotificationClient::session_
        // notification`). The parent's UI reconstructs source from the
        // chunk's `meta` (set by `send_notify_text` /
        // `send_notify_tool_call`) and renders the heading itself.
        if let Some(forward) = event_to_forward(event, source) {
            let _ = self.tx.send(forward);
        }
    }
}

/// Map an `AgentEvent` to the `AcpForward` it should produce, or `None`
/// when the event carries no forwardable payload (e.g. empty text). Kept
/// as a free function so `AcpChunkSink::emit` stays a trivial dispatch.
pub(crate) fn event_to_forward(
    event: AgentEvent,
    source: Option<AgentSource>,
) -> Option<AcpForward> {
    match event {
        AgentEvent::Model(ModelEvent::MessageChunk { blocks }) => {
            let text: String = blocks
                .iter()
                .filter_map(|b| match b {
                    harnx_core::event::ContentBlock::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect();
            (!text.is_empty()).then(|| AcpForward::Text(text, source))
        }
        AgentEvent::Model(ModelEvent::Final { output, .. }) if !output.is_empty() => {
            Some(AcpForward::Text(output, source))
        }
        AgentEvent::Model(ModelEvent::Error(err)) if !err.is_empty() => {
            Some(AcpForward::Text(format!("error: {err}"), source))
        }
        AgentEvent::User(UserEvent::Message { content }) if !content.is_empty() => {
            Some(AcpForward::UserText(content, source))
        }
        AgentEvent::Tool(ToolEvent::Started {
            id,
            name,
            input,
            markdown,
            ..
        }) => Some(AcpForward::ToolCall {
            id,
            name,
            input,
            markdown,
            source,
        }),
        AgentEvent::Tool(ToolEvent::Update {
            id,
            markdown,
            status,
            ..
        }) => Some(AcpForward::ToolUpdate {
            id,
            markdown,
            status,
            source,
        }),
        AgentEvent::Tool(ToolEvent::Completed {
            id,
            output,
            markdown,
            ..
        }) => Some(AcpForward::ToolCompleted {
            id,
            output,
            markdown,
            source,
        }),
        // Only surface Warning/Error notices to ACP clients — these carry the
        // #990 MCP server restart/death signals. Info notices are a
        // presentation-layer artifact (nested sub-agent activity headings
        // routed through NestedAcpEvent::Text → NoticeEvent::Info) and must
        // NOT leak into the ACP message stream, or they corrupt the transcript
        // (regression guarded by tmux_e2e::nested_sub_agent_activity_no_duplicates).
        AgentEvent::Notice(notice) => {
            let forwarded = match notice {
                NoticeEvent::Info(_) => None,
                NoticeEvent::Warning(m) => (!m.is_empty()).then_some(("⚠", m)),
                NoticeEvent::Error(m) => (!m.is_empty()).then_some(("🔴", m)),
            };
            forwarded.map(|(prefix, msg)| AcpForward::Text(format!("{prefix} {msg}"), source))
        }
        _ => None,
    }
}

/// Per-session context owned by HarnxAgent.
///
/// Holds a forked GlobalConfig with its OWN McpManager/AcpManager, set up once
/// at session creation (or lazy resume). The managers persist for the lifetime
/// of this SessionContext, so MCP subprocesses stay alive across prompts.
///
/// Mirrors the NATS worker's per-session config pattern: fork once, reuse
/// for every turn in the session.
pub struct SessionContext {
    /// Session ID (matches the on-disk session log filename).
    pub session_id: String,
    /// Forked config with agent+session bound, owning its own managers.
    pub config: GlobalConfig,
    /// Abort signal for cancellation.
    pub abort_signal: AbortSignal,
    /// Fires when the session receives an ACP `session/cancel` notification.
    pub cancel_notify: Arc<tokio::sync::Notify>,
    /// Serializes prompts targeting this session.
    pub prompt_lock: Arc<tokio::sync::Mutex<()>>,
    /// Last activity timestamp for idle reaper.
    last_activity: parking_lot::Mutex<Instant>,
    /// Test override to force session into idle-expired state without relying
    /// on backdating `Instant` before monotonic clock origin.
    force_idle: AtomicBool,
}

impl Drop for SessionContext {
    fn drop(&mut self) {
        debug!(
            "SessionContext drop: session_id={} — managers will be torn down",
            self.session_id
        );
        // The Arc<SessionContext> drop triggers Config drop, which drops
        // McpManager → clients.clear() → drops Arc<McpClient> → stdin closes
        // → MCP subprocess exits.
    }
}

impl SessionContext {
    fn new(session_id: String, config: GlobalConfig) -> Self {
        Self {
            session_id,
            config,
            abort_signal: AbortSignalInner::new(),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
            prompt_lock: Arc::new(tokio::sync::Mutex::new(())),
            last_activity: parking_lot::Mutex::new(Instant::now()),
            force_idle: AtomicBool::new(false),
        }
    }

    /// Update the last activity timestamp. Called whenever the session sees
    /// activity: at prompt start, when a prompt stops (completion OR
    /// cancellation), and on a `session/cancel` notification.
    fn touch(&self) {
        self.force_idle.store(false, Ordering::Relaxed);
        *self.last_activity.lock() = Instant::now();
    }

    /// Test-only: force idle-expired state without relying on subtracting
    /// past monotonic clock origin on freshly booted machines.
    #[cfg(test)]
    fn mark_idle_for_test(&self) {
        self.force_idle.store(true, Ordering::Relaxed);
    }

    /// Check if this session is currently running a prompt.
    fn is_running(&self) -> bool {
        // If we can NOT acquire the lock, a prompt is running.
        // try_lock returns Ok(MutexGuard) if successful (no lock held),
        // Err(TryLockError) if locked (prompt running).
        self.prompt_lock.try_lock().is_err()
    }

    /// Check if the idle TTL has elapsed.
    fn is_idle_expired(&self) -> bool {
        self.force_idle.load(Ordering::Relaxed)
            || self.last_activity.lock().elapsed() >= SESSION_IDLE_TTL
    }

    /// Whether the idle reaper should evict this session: it must be idle past
    /// the TTL AND have no in-flight prompt. A running prompt (prompt_lock held)
    /// is never reaped, so its live MCP subprocesses aren't pulled out from
    /// under it.
    fn should_reap(&self) -> bool {
        self.is_idle_expired() && !self.is_running()
    }
}

pub struct HarnxAgent {
    agent_name: String,
    /// Base config to fork from for each new session.
    base_config: GlobalConfig,
    /// Active sessions keyed by session_id.
    sessions: Arc<tokio::sync::Mutex<HashMap<String, Arc<SessionContext>>>>,
    connection: Arc<tokio::sync::Mutex<Option<acp::ConnectionTo<acp::Client>>>>,
}

impl HarnxAgent {
    pub fn new(agent_name: String, config: GlobalConfig) -> Self {
        Self {
            agent_name,
            base_config: config,
            sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            connection: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    pub async fn set_connection(&self, conn: acp::ConnectionTo<acp::Client>) {
        *self.connection.lock().await = Some(conn);
    }

    /// Build a per-session config: fork base, set agent+session.
    /// Called once at session creation (new_session) and at lazy resume.
    fn build_session_config(&self, session_id: &str) -> acp::Result<GlobalConfig> {
        let config = harnx_session::fork_prompt_config(&self.base_config.read().clone());
        {
            let mut cfg = config.write();
            cfg.use_agent_by_name(&self.agent_name)
                .map_err(|e| acp::Error::new(-32603, format!("Failed to set agent: {e}")))?;
            cfg.use_session(Some(session_id))
                .map_err(|e| acp::Error::new(-32603, format!("Failed to use session: {e}")))?;
        }
        Ok(config)
    }

    /// Build the local-agent turn input from an already-configured session.
    fn build_local_input_from_config(
        &self,
        session_config: &GlobalConfig,
        _session_key: &str,
        prompt_text: &str,
    ) -> acp::Result<harnx_runtime::config::Input> {
        // Agent and session are already set on session_config.
        // Build a fresh agent for the input.
        let mut agent = session_config
            .read()
            .retrieve_agent(&self.agent_name)
            .map_err(|e| acp::Error::new(-32603, format!("Failed to retrieve agent: {e}")))?;
        if let Err(e) = harnx_runtime::config::agent::resolve_variables(&mut agent) {
            warn!(
                "Failed to resolve variables for agent '{}': {e}",
                self.agent_name
            );
        }

        let mut input = harnx_runtime::config::input::from_str(session_config, prompt_text, None);
        harnx_runtime::config::input::set_agent(&mut input, session_config, agent.into_config());
        Ok(input)
    }
}

impl HarnxAgent {
    async fn initialize(&self, args: InitializeRequest) -> acp::Result<InitializeResponse> {
        Ok(InitializeResponse::new(args.protocol_version)
            .agent_capabilities(AgentCapabilities::new())
            .agent_info(
                Implementation::new("harnx", env!("CARGO_PKG_VERSION"))
                    .title(self.agent_name.clone()),
            ))
    }

    async fn authenticate(&self, _args: AuthenticateRequest) -> acp::Result<AuthenticateResponse> {
        Ok(AuthenticateResponse::default())
    }

    async fn new_session(&self, _args: NewSessionRequest) -> acp::Result<NewSessionResponse> {
        // Create a new session id and config (managers set up once here).
        let session_id = {
            let mut base = self.base_config.write();
            if base.session.is_some() {
                base.exit_session()
                    .map_err(|e| acp::Error::new(-32603, format!("Failed to exit session: {e}")))?;
            }
            base.use_agent_by_name(&self.agent_name)
                .map_err(|e| acp::Error::new(-32603, format!("Failed to set agent: {e}")))?;
            base.use_session(None)
                .map_err(|e| acp::Error::new(-32603, format!("Failed to create session: {e}")))?;
            let id = base
                .session
                .as_ref()
                .expect("session must exist after use_session(None)")
                .id
                .clone();
            base.exit_session()
                .map_err(|e| acp::Error::new(-32603, format!("Failed to persist session: {e}")))?;
            id
        };

        // Build the per-session config with its own managers.
        let session_config = self.build_session_config(&session_id)?;
        let ctx = Arc::new(SessionContext::new(session_id.clone(), session_config));

        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), ctx.clone());

        // Start the idle reaper task (spawns once per session, exits when ctx drops).
        self.spawn_idle_reaper(ctx);

        Ok(NewSessionResponse::new(SessionId::new(session_id)))
    }

    async fn prompt(&self, args: PromptRequest) -> acp::Result<PromptResponse> {
        let session_key = args.session_id.0.to_string();
        let prompt_text = prompt_blocks_to_text(&args.prompt);

        // Look up or lazily build the SessionContext.
        let session_ctx = self.get_or_build_session(&session_key).await?;

        // Update activity timestamp.
        session_ctx.touch();

        // Hold the per-session lock for the whole prompt.
        let _prompt_guard = session_ctx.prompt_lock.clone().lock_owned().await;

        // Reset abort signal after acquiring lock.
        session_ctx.abort_signal.reset();

        // P4.2: remote agents (`agent@cluster`) run in thin-client mode.
        let remote_agent = parse_remote_agent(&self.agent_name);

        // Use the session's own config (managers already initialized).
        let prompt_config = session_ctx.config.clone();

        let input = if remote_agent.is_none() {
            Some(self.build_local_input_from_config(&prompt_config, &session_key, &prompt_text)?)
        } else {
            None
        };

        // Install a per-prompt AgentEventSink for streaming chunks.
        let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel::<AcpForward>();
        let sink: Arc<dyn harnx_core::event::AgentEventSink> =
            Arc::new(AcpChunkSink { tx: chunk_tx });

        // Spawn local task to drain chunk_rx → session_notification.
        let connection_for_fwd = self.connection.lock().await.clone();
        let session_key_for_fwd = session_key.clone();

        fn send_notify_text(
            conn: &Option<acp::ConnectionTo<acp::Client>>,
            session_key: &str,
            text: String,
            source: Option<AgentSource>,
        ) {
            if text.is_empty() {
                return;
            }
            if let Some(conn) = conn.as_ref() {
                let sid = session_key.to_string();
                let mut chunk = ContentChunk::new(text.into());
                if let Some(source) = source.as_ref() {
                    if let Some(meta) = meta_from_source(source) {
                        chunk = chunk.meta(meta);
                    }
                }
                let notification = SessionNotification::new(
                    SessionId::new(sid),
                    SessionUpdate::AgentMessageChunk(chunk),
                );
                let _ = conn.send_notification(notification);
            }
        }

        fn send_notify_user_text(
            conn: &Option<acp::ConnectionTo<acp::Client>>,
            session_key: &str,
            text: String,
            source: Option<AgentSource>,
        ) {
            if text.is_empty() {
                return;
            }
            if let Some(conn) = conn.as_ref() {
                let sid = session_key.to_string();
                let mut chunk = ContentChunk::new(text.into());
                if let Some(source) = source.as_ref() {
                    if let Some(meta) = meta_from_source(source) {
                        chunk = chunk.meta(meta);
                    }
                }
                let notification = SessionNotification::new(
                    SessionId::new(sid),
                    SessionUpdate::UserMessageChunk(chunk),
                );
                let _ = conn.send_notification(notification);
            }
        }

        fn send_notify_tool_call(
            conn: &Option<acp::ConnectionTo<acp::Client>>,
            session_key: &str,
            id: String,
            name: String,
            input: serde_json::Value,
            markdown: Option<String>,
            source: Option<AgentSource>,
        ) {
            if let Some(conn) = conn.as_ref() {
                let sid = session_key.to_string();
                let tool_call_id = if id.is_empty() { name.clone() } else { id };
                let mut tc = ToolCall::new(tool_call_id, name).raw_input(input);
                let mut meta_map: Option<serde_json::Map<String, serde_json::Value>> = None;
                if let Some(source) = source.as_ref() {
                    meta_map = meta_from_source(source);
                }
                if let Some(md) = markdown.filter(|t| !t.is_empty()) {
                    let map = meta_map.get_or_insert_with(serde_json::Map::new);
                    map.insert("harnx:markdown".to_string(), serde_json::Value::String(md));
                }
                if let Some(map) = meta_map {
                    tc = tc.meta(map);
                }
                let notification =
                    SessionNotification::new(SessionId::new(sid), SessionUpdate::ToolCall(tc));
                let _ = conn.send_notification(notification);
            }
        }

        fn send_notify_tool_update(
            conn: &Option<acp::ConnectionTo<acp::Client>>,
            session_key: &str,
            id: String,
            markdown: Option<String>,
            status: Option<harnx_core::event::ToolStatus>,
            source: Option<AgentSource>,
        ) {
            if let Some(conn) = conn.as_ref() {
                let sid = session_key.to_string();
                let acp_status = status.map(|s| match s {
                    harnx_core::event::ToolStatus::Pending => ToolCallStatus::Pending,
                    harnx_core::event::ToolStatus::InProgress => ToolCallStatus::InProgress,
                    harnx_core::event::ToolStatus::Completed => ToolCallStatus::Completed,
                    harnx_core::event::ToolStatus::Failed => ToolCallStatus::Failed,
                });
                let mut fields = ToolCallUpdateFields::new();
                if let Some(s) = acp_status {
                    fields = fields.status(s);
                }
                if let Some(md) = markdown.filter(|t| !t.is_empty()) {
                    fields = fields.title(md);
                }
                let mut tcu = ToolCallUpdate::new(id, fields);
                if let Some(source) = source.as_ref() {
                    if let Some(meta) = meta_from_source(source) {
                        tcu = tcu.meta(meta);
                    }
                }
                let notification = SessionNotification::new(
                    SessionId::new(sid),
                    SessionUpdate::ToolCallUpdate(tcu),
                );
                let _ = conn.send_notification(notification);
            }
        }

        fn send_notify_tool_completed(
            conn: &Option<acp::ConnectionTo<acp::Client>>,
            session_key: &str,
            id: String,
            output: serde_json::Value,
            markdown: Option<String>,
            source: Option<AgentSource>,
        ) {
            if let Some(conn) = conn.as_ref() {
                let sid = session_key.to_string();
                let mut fields = ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .raw_output(output);
                if let Some(md) = markdown.filter(|t| !t.is_empty()) {
                    fields = fields.title(md);
                }
                let mut tcu = ToolCallUpdate::new(id, fields);
                if let Some(source) = source.as_ref() {
                    if let Some(meta) = meta_from_source(source) {
                        tcu = tcu.meta(meta);
                    }
                }
                let notification = SessionNotification::new(
                    SessionId::new(sid),
                    SessionUpdate::ToolCallUpdate(tcu),
                );
                let _ = conn.send_notification(notification);
            }
        }

        fn send_notify_forward(
            conn: &Option<acp::ConnectionTo<acp::Client>>,
            session_key: &str,
            forward: AcpForward,
        ) {
            match forward {
                AcpForward::Text(text, source) => {
                    send_notify_text(conn, session_key, text, source);
                }
                AcpForward::UserText(text, source) => {
                    send_notify_user_text(conn, session_key, text, source);
                }
                AcpForward::ToolCall {
                    id,
                    name,
                    input,
                    markdown,
                    source,
                } => {
                    send_notify_tool_call(conn, session_key, id, name, input, markdown, source);
                }
                AcpForward::ToolUpdate {
                    id,
                    markdown,
                    status,
                    source,
                } => {
                    send_notify_tool_update(conn, session_key, id, markdown, status, source);
                }
                AcpForward::ToolCompleted {
                    id,
                    output,
                    markdown,
                    source,
                } => {
                    send_notify_tool_completed(conn, session_key, id, output, markdown, source);
                }
            }
        }

        // Forward task: drain chunk_rx → session_notification.
        let fwd_task = tokio::spawn(async move {
            while let Some(forward) = chunk_rx.recv().await {
                send_notify_forward(&connection_for_fwd, &session_key_for_fwd, forward);
            }
        });

        // Build the agent loop context from the session's config.
        let abort_signal = session_ctx.abort_signal.clone();
        let cancel_notify = session_ctx.cancel_notify.clone();

        let loop_ctx = harnx_session::build_context(
            prompt_config.clone(),
            None,
            abort_signal.clone(),
            None,
            None,
        );

        // Cancel listener: wait on cancel_notify, fire abort.
        let cancel_abort = abort_signal.clone();
        let cancel_listener = tokio::spawn(async move {
            cancel_notify.notified().await;
            cancel_abort.set_ctrlc();
        });

        // Grace period after abort before hard cancellation.
        let abort_for_grace = abort_signal.clone();
        let grace_cancel = async move {
            harnx_core::abort::wait_abort_signal(&abort_for_grace).await;
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        };

        // Run the agent loop.
        let remote_sink = sink.clone();
        let loop_result = harnx_core::sink::with_agent_event_sink(sink, async {
            match &remote_agent {
                Some((agent, cluster)) => {
                    let thin_cfg = harnx_runtime::ThinClientConfig {
                        cluster: cluster.clone(),
                        agent: agent.clone(),
                        session_id: Some(session_key.clone()),
                    };
                    let mut run_remote = Box::pin(async {
                        let session = harnx_runtime::ThinClientSession::from_global_config(
                            thin_cfg,
                            &prompt_config,
                            abort_signal.clone(),
                        )
                        .await?;
                        session
                            .run_turn(&prompt_text, remote_sink, None)
                            .await
                            .map(|_| ())
                    });
                    tokio::select! {
                        r = &mut run_remote => Some(r),
                        _ = grace_cancel => None,
                    }
                }
                None => {
                    let run_local = async {
                        harnx_runtime::run_agent_loop_with_local_handoff(
                            &loop_ctx,
                            input.expect("local mode always builds input"),
                        )
                        .await
                    };
                    let mut run_local = Box::pin(run_local);
                    tokio::select! {
                        r = &mut run_local => Some(r),
                        _ = grace_cancel => None,
                    }
                }
            }
        })
        .await;

        // Refresh activity when the turn STOPS being active — on EVERY exit
        // path (normal completion AND cancellation), not just at prompt start.
        // A turn can run longer than SESSION_IDLE_TTL; touching only at start
        // would leave last_activity stale, so the very next reaper tick after a
        // long turn would evict the just-active session and tear down its warm
        // MCP subprocesses. Placing this before the cancellation early-return
        // keeps the session alive for the full TTL measured from when it last
        // finished doing work, whether it completed or was cancelled.
        session_ctx.touch();

        let Some(loop_result) = loop_result else {
            cancel_listener.abort();
            fwd_task.abort();
            return Ok(PromptResponse::new(StopReason::Cancelled));
        };

        cancel_listener.abort();
        drop(loop_ctx);
        let _ = fwd_task.await;

        prompt_stop_response(loop_result, &abort_signal)
    }

    async fn cancel(&self, args: CancelNotification) -> acp::Result<()> {
        let session_id = args.session_id.0;
        let sessions = self.sessions.lock().await;
        let session = sessions
            .get(session_id.as_ref())
            .ok_or_else(acp::Error::invalid_params)?;
        // A cancel is direct user interaction — count it as activity so the
        // idle reaper doesn't evict a session the user is actively steering.
        session.touch();
        session.abort_signal.set_ctrlc();
        session.cancel_notify.notify_one();
        Ok(())
    }
}

impl HarnxAgent {
    /// Get an existing session or lazily build one from disk.
    async fn get_or_build_session(&self, session_id: &str) -> acp::Result<Arc<SessionContext>> {
        // First check if it's already in memory.
        {
            let sessions = self.sessions.lock().await;
            if let Some(ctx) = sessions.get(session_id) {
                return Ok(ctx.clone());
            }
        }

        // Session not in memory. Check if it exists on disk.
        let session_path = self.base_config.read().session_file(session_id);
        if !tokio::fs::try_exists(&session_path).await.unwrap_or(false) {
            return Err(acp::Error::invalid_params());
        }

        // Lazy rebuild: fork config, load session from disk.
        info!("Lazy rebuilding session {} from disk", session_id);
        let session_config = self.build_session_config(session_id)?;
        let ctx = Arc::new(SessionContext::new(session_id.to_string(), session_config));

        // Insert and start reaper.
        let mut sessions = self.sessions.lock().await;
        // Check again under lock (race with concurrent prompt).
        if let Some(existing) = sessions.get(session_id) {
            return Ok(existing.clone());
        }
        sessions.insert(session_id.to_string(), ctx.clone());
        drop(sessions);

        self.spawn_idle_reaper(ctx.clone());

        Ok(ctx)
    }

    /// Spawn an idle reaper task for a session.
    /// The task periodically checks if the session is idle past TTL
    /// and evicts it from the map. Exits when the SessionContext is dropped.
    fn spawn_idle_reaper(&self, ctx: Arc<SessionContext>) {
        let sessions = self.sessions.clone();
        let session_id = ctx.session_id.clone();

        // Hold a weak reference: if the session is evicted by another path,
        // the reaper task should exit.
        let weak_ctx = Arc::downgrade(&ctx);
        drop(ctx); // Don't hold an extra strong ref.

        tokio::spawn(async move {
            // Check every minute.
            let check_interval = Duration::from_secs(60);
            loop {
                tokio::time::sleep(check_interval).await;

                // If the SessionContext was already dropped, exit.
                let Some(ctx) = weak_ctx.upgrade() else {
                    debug!("Idle reaper: session {} already dropped", session_id);
                    break;
                };

                // Evict only if idle past the TTL and no prompt is in flight.
                if ctx.should_reap() {
                    info!(
                        "Idle reaper: evicting session {} (idle > {:?})",
                        session_id, SESSION_IDLE_TTL
                    );
                    let mut sessions = sessions.lock().await;
                    // Remove only if it's still this exact session and still idle.
                    if let Some(existing) = sessions.get(&session_id) {
                        if Arc::ptr_eq(existing, &ctx) {
                            if ctx.should_reap() {
                                sessions.remove(&session_id);
                                drop(sessions);
                                // ctx drops here, triggering manager teardown.
                                break;
                            }
                            continue;
                        }
                    }
                    drop(sessions);
                }
            }
        });
    }
}

/// Build the `meta` map that `AcpNotificationClient::resolve_notification_source`
/// reads on the client side to reconstruct `AgentSource`. Keys must match
/// `agent_from_meta_value` / `session_from_meta_value` / `model_from_meta_value`
/// in `harnx-acp::client`.
fn meta_from_source(source: &AgentSource) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut map = serde_json::Map::new();
    map.insert(
        "agent".to_string(),
        serde_json::Value::String(source.agent.clone()),
    );
    if let Some(session_id) = &source.session_id {
        map.insert(
            "session".to_string(),
            serde_json::Value::String(session_id.clone()),
        );
    }
    if let Some(model) = &source.model {
        map.insert(
            "harnx:model".to_string(),
            serde_json::Value::String(model.clone()),
        );
    }
    Some(map)
}

/// Map a completed agent-loop result to the ACP prompt response, treating
/// errors that coincide with an aborted signal as a cancellation.
fn prompt_stop_response(
    loop_result: anyhow::Result<()>,
    abort_signal: &AbortSignal,
) -> acp::Result<PromptResponse> {
    match loop_result {
        Ok(()) => Ok(PromptResponse::new(StopReason::EndTurn)),
        Err(_e) if abort_signal.aborted() => Ok(PromptResponse::new(StopReason::Cancelled)),
        Err(e) => Err(acp::Error::new(-32603, format!("Agent loop error: {e:#}"))),
    }
}

/// Join the text of all ACP prompt content blocks into a single string.
fn prompt_blocks_to_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .map(content_block_to_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Detect a remote (`agent@cluster`) thin-client ref, returning the owned
/// `(agent, cluster)` pair. Local refs return `None`.
fn parse_remote_agent(agent_name: &str) -> Option<(String, String)> {
    match harnx_core::agent_ref::AgentRef::parse(agent_name) {
        harnx_core::agent_ref::AgentRef::Remote { agent, cluster } => {
            Some((agent.into_owned(), cluster.into_owned()))
        }
        harnx_core::agent_ref::AgentRef::Local(_) => None,
    }
}

fn content_block_to_text(content: &ContentBlock) -> String {
    match content {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::ResourceLink(link) => link.uri.to_string(),
        ContentBlock::Image(_) => "<image>".to_string(),
        ContentBlock::Audio(_) => "<audio>".to_string(),
        ContentBlock::Resource(_) => "<resource>".to_string(),
        _ => String::new(),
    }
}
