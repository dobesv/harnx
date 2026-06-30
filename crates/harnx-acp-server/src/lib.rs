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
use agent_client_protocol::schema::*;
use harnx_hooks::{AsyncHookManager, PersistentHookManager};
use std::{collections::HashMap, sync::Arc};

use harnx_core::event::{AgentEvent, AgentSource, ModelEvent, ToolEvent, UserEvent};
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::utils::{AbortSignal, AbortSignalInner};

/// Update payloads forwarded from the per-prompt `AcpChunkSink` to the
/// local `fwd_task`. Each variant carries the original `AgentSource` so
/// the forwarded `SessionNotification` can attach `meta` describing
/// which agent (parent vs. some sub-agent) actually produced the event;
/// the parent's `AcpNotificationClient::resolve_notification_source`
/// reads that meta to render the right `> agent ▸ session` heading.
enum AcpForward {
    /// Text chunk for `SessionUpdate::AgentMessageChunk`.
    Text(String, Option<AgentSource>),
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
/// Forwards events from the unified `run_agent_loop` through a channel
/// to a spawned local task that calls `session_notification`. The
/// channel is required because the ACP `connection` is `Rc<...>` (`!Send`)
/// and can't be captured in the sink itself.
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
        // chunk's `meta` (set by `spawn_notify_text` /
        // `spawn_notify_tool_call`) and renders the heading itself.
        match event {
            AgentEvent::Model(ModelEvent::MessageChunk { blocks }) => {
                let text: String = blocks
                    .iter()
                    .filter_map(|b| match b {
                        harnx_core::event::ContentBlock::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect();
                if !text.is_empty() {
                    let _ = self.tx.send(AcpForward::Text(text, source));
                }
            }
            AgentEvent::Model(ModelEvent::Final { output, .. }) if !output.is_empty() => {
                let _ = self.tx.send(AcpForward::Text(output, source));
            }
            AgentEvent::User(UserEvent::Message { content }) if !content.is_empty() => {
                let _ = self.tx.send(AcpForward::Text(content, source));
            }
            AgentEvent::Tool(ToolEvent::Started {
                id,
                name,
                input,
                markdown,
                ..
            }) => {
                let _ = self.tx.send(AcpForward::ToolCall {
                    id,
                    name,
                    input,
                    markdown,
                    source,
                });
            }
            AgentEvent::Tool(ToolEvent::Update {
                id,
                markdown,
                status,
                ..
            }) => {
                let _ = self.tx.send(AcpForward::ToolUpdate {
                    id,
                    markdown,
                    status,
                    source,
                });
            }
            AgentEvent::Tool(ToolEvent::Completed {
                id,
                output,
                markdown,
                ..
            }) => {
                let _ = self.tx.send(AcpForward::ToolCompleted {
                    id,
                    output,
                    markdown,
                    source,
                });
            }
            _ => {}
        }
    }
}

pub struct HarnxAgent {
    agent_name: String,
    config: GlobalConfig,
    sessions: Arc<tokio::sync::Mutex<HashMap<String, HarnxSession>>>,
    connection: Arc<tokio::sync::Mutex<Option<acp::ConnectionTo<acp::Client>>>>,
}

#[derive(Clone)]
struct HarnxSession {
    abort_signal: AbortSignal,
    /// Fires when the session receives an ACP `session/cancel` notification.
    /// We use `notify_one` (rather than `notify_waiters`) so a cancel that
    /// arrives in the tiny window between the prompt handler entering and
    /// its `.notified()` future being polled still fires — the permit is
    /// held until the next listener observes it.
    ///
    /// Known limitation: a cancel that arrives AFTER a prompt returns and
    /// BEFORE the next prompt starts will be consumed by the next prompt's
    /// first `.notified()` poll. Drain-at-entry was attempted but racing
    /// the drain against a concurrent cancel is itself unsound (polling a
    /// Notified registers a waiter that can absorb a concurrent
    /// notify_one even after we drop it). In practice cancel notifications
    /// are only sent while a prompt is active, so this case isn't
    /// exercised by any test.
    cancel_notify: Arc<tokio::sync::Notify>,
    /// Serializes prompts that target THIS session. Prompts to *different*
    /// sessions still run fully concurrently (each holds its own lock).
    /// Without this, two concurrent prompts with the same `session_id` would
    /// each fork their own in-memory `Session`, independently load the same
    /// on-disk log, and clobber each other's appends (last-writer-wins). The
    /// lock makes the load → run → persist cycle for one session atomic with
    /// respect to other prompts on the same session.
    prompt_lock: Arc<tokio::sync::Mutex<()>>,
}

impl HarnxAgent {
    pub fn new(agent_name: String, config: GlobalConfig) -> Self {
        Self {
            agent_name,
            config,
            sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            connection: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    pub async fn set_connection(&self, conn: acp::ConnectionTo<acp::Client>) {
        *self.connection.lock().await = Some(conn);
    }

    /// Build the local-agent turn input: select the agent + session on the
    /// forked prompt config, resolve agent variables, and construct the `Input`
    /// from the user's prompt text. Only used for local agent refs (remote
    /// thin-client mode drives the turn over NATS instead).
    fn build_local_input(
        &self,
        prompt_config: &GlobalConfig,
        session_key: &str,
        prompt_text: &str,
    ) -> acp::Result<harnx_runtime::config::Input> {
        {
            let mut config = prompt_config.write();
            config
                .use_agent_by_name(&self.agent_name)
                .map_err(|e| acp::Error::new(-32603, format!("Failed to set agent: {e}")))?;
            config
                .use_session(Some(session_key))
                .map_err(|e| acp::Error::new(-32603, format!("Failed to use session: {e}")))?;
        }

        // Build a fresh agent for the input. The forked prompt config keeps
        // per-request session state isolated while still sharing Arc-backed
        // runtime resources cloned from the base config.
        let mut agent = prompt_config
            .read()
            .retrieve_agent(&self.agent_name)
            .map_err(|e| acp::Error::new(-32603, format!("Failed to retrieve agent: {e}")))?;
        if let Err(e) = harnx_runtime::config::agent::resolve_variables(&mut agent) {
            warn!(
                "Failed to resolve variables for agent '{}': {e}",
                self.agent_name
            );
        }

        let mut input = harnx_runtime::config::input::from_str(prompt_config, prompt_text, None);
        harnx_runtime::config::input::set_agent(&mut input, prompt_config, agent.into_config());
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
        let session_id;
        {
            let mut config = self.config.write();
            if config.session.is_some() {
                config
                    .exit_session()
                    .map_err(|e| acp::Error::new(-32603, format!("Failed to exit session: {e}")))?;
            }
            config
                .use_agent_by_name(&self.agent_name)
                .map_err(|e| acp::Error::new(-32603, format!("Failed to set agent: {e}")))?;
            config
                .use_session(None)
                .map_err(|e| acp::Error::new(-32603, format!("Failed to create session: {e}")))?;
            session_id = config
                .session
                .as_ref()
                .expect("session must exist after use_session(None)")
                .id
                .clone();
            config
                .exit_session()
                .map_err(|e| acp::Error::new(-32603, format!("Failed to persist session: {e}")))?;
        }
        let session = HarnxSession {
            abort_signal: AbortSignalInner::new(),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
            prompt_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), session);
        Ok(NewSessionResponse::new(SessionId::new(session_id)))
    }

    async fn prompt(&self, args: PromptRequest) -> acp::Result<PromptResponse> {
        let session_key = args.session_id.0.to_string();
        let prompt_text = prompt_blocks_to_text(&args.prompt);

        let (abort_signal, cancel_notify, prompt_lock) = {
            let sessions = self.sessions.lock().await;
            let session = sessions
                .get(session_key.as_str())
                .ok_or_else(acp::Error::invalid_params)?;
            (
                session.abort_signal.clone(),
                session.cancel_notify.clone(),
                session.prompt_lock.clone(),
            )
        };

        // Hold the per-session lock for the whole prompt so two prompts to the
        // SAME session_id can't fork independent in-memory sessions and clobber
        // each other's on-disk transcript. The `sessions` map lock above is
        // already released, so prompts to different sessions never contend
        // here. Guard is kept alive until the function returns.
        let _prompt_guard = prompt_lock.lock_owned().await;

        // Reset the abort signal only AFTER winning the per-session lock. If we
        // reset before acquiring it, a queued same-session prompt could clear a
        // cancellation that the still-running prompt holding the lock has not
        // yet observed, effectively un-cancelling it against user intent.
        abort_signal.reset();

        // P4.2: remote agents (`agent@cluster`) run in thin-client mode — the
        // turn is driven over NATS by a worker daemon, not by a local agent
        // loop. Detect from the configured agent name; local refs are unchanged.
        let remote_agent = parse_remote_agent(&self.agent_name);

        let prompt_config: GlobalConfig = Arc::new(parking_lot::RwLock::new({
            let shared_config = self.config.read();
            shared_config.fork_session_scope()
        }));
        // Local-agent setup (agent resolution + input building) only applies to
        // local refs. For remote thin-client mode the worker resolves the agent
        // and the input is the user's prompt text posted to the NATS log.
        let input = if remote_agent.is_none() {
            Some(self.build_local_input(&prompt_config, &session_key, &prompt_text)?)
        } else {
            None
        };

        // Install a per-prompt AgentEventSink for streaming chunks
        // (MessageChunk events) and tool calls (ToolEvent::Started).
        // Nested ACP activity from sub-agent delegations also flows through
        // this sink because `AcpManager::call_tool` captures and reinstalls
        // the current task-local sink inside its spawned forwarder task.
        let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel::<AcpForward>();
        let sink: Arc<dyn harnx_core::event::AgentEventSink> =
            Arc::new(AcpChunkSink { tx: chunk_tx });

        // Spawn local task to drain chunk_rx → session_notification.
        let connection_for_fwd = self.connection.lock().await.clone();
        let session_key_for_fwd = session_key.clone();
        // Helpers: fire a session_notification without blocking the LocalSet
        // thread. Each notification is spawned as its own local task so
        // run_agent_loop / execute_tool_round are never starved waiting for
        // the parent to acknowledge a notification write.
        // Build the `meta` payload that `AcpNotificationClient::resolve_
        // notification_source` reads to determine `AgentSource`. Without
        // these fields the parent infers source from the connection's
        // agent_name, which (a) loses sub-agent identity when this
        // server is forwarding a nested chunk and (b) prevents
        // `render_ui_output_heading` from emitting `> agent ▸ session`.
        // `agent_from_meta_value` / `session_from_meta_value` in
        // `harnx-acp::client` read `agent` and `session` keys (no
        // namespace prefix). Match those exactly so the parent recovers
        // sub-agent identity.

        fn spawn_notify_text(
            conn: &Option<acp::ConnectionTo<acp::Client>>,
            session_key: &str,
            text: String,
            source: Option<AgentSource>,
        ) {
            if text.is_empty() {
                return;
            }
            if let Some(conn) = conn.clone() {
                let sid = session_key.to_string();
                tokio::task::spawn_local(async move {
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
                });
            }
        }

        fn spawn_notify_tool_call(
            conn: &Option<acp::ConnectionTo<acp::Client>>,
            session_key: &str,
            id: String,
            name: String,
            input: serde_json::Value,
            markdown: Option<String>,
            source: Option<AgentSource>,
        ) {
            if let Some(conn) = conn.clone() {
                let sid = session_key.to_string();
                tokio::task::spawn_local(async move {
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
                });
            }
        }

        fn spawn_notify_tool_update(
            conn: &Option<acp::ConnectionTo<acp::Client>>,
            session_key: &str,
            id: String,
            markdown: Option<String>,
            status: Option<harnx_core::event::ToolStatus>,
            source: Option<AgentSource>,
        ) {
            if let Some(conn) = conn.clone() {
                let sid = session_key.to_string();
                tokio::task::spawn_local(async move {
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
                });
            }
        }

        fn spawn_notify_tool_completed(
            conn: &Option<acp::ConnectionTo<acp::Client>>,
            session_key: &str,
            id: String,
            output: serde_json::Value,
            markdown: Option<String>,
            source: Option<AgentSource>,
        ) {
            if let Some(conn) = conn.clone() {
                let sid = session_key.to_string();
                tokio::task::spawn_local(async move {
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
                });
            }
        }

        fn spawn_notify_forward(
            conn: &Option<acp::ConnectionTo<acp::Client>>,
            session_key: &str,
            update: AcpForward,
        ) {
            match update {
                AcpForward::Text(text, source) => {
                    spawn_notify_text(conn, session_key, text, source)
                }
                AcpForward::ToolCall {
                    id,
                    name,
                    input,
                    markdown,
                    source,
                } => spawn_notify_tool_call(conn, session_key, id, name, input, markdown, source),
                AcpForward::ToolUpdate {
                    id,
                    markdown,
                    status,
                    source,
                } => spawn_notify_tool_update(conn, session_key, id, markdown, status, source),
                AcpForward::ToolCompleted {
                    id,
                    output,
                    markdown,
                    source,
                } => spawn_notify_tool_completed(conn, session_key, id, output, markdown, source),
            }
        }

        let fwd_task = tokio::task::spawn_local(async move {
            while let Some(update) = chunk_rx.recv().await {
                spawn_notify_forward(&connection_for_fwd, &session_key_for_fwd, update);
            }
        });

        // We deliberately do NOT register an `on_text_response` here:
        // streaming `MessageChunk` events already flow through the
        // `AcpChunkSink` / `chunk_rx` / `fwd_task` chain, which
        // session_notification each chunk to the parent. Adding an
        // `on_text_response` would re-emit the same final text and the
        // parent's transcript would render the assistant's reply twice.

        let loop_ctx = harnx_runtime::AgentLoopContext {
            config: prompt_config.clone(),
            abort_signal: abort_signal.clone(),
            async_manager: Arc::new(tokio::sync::Mutex::new(AsyncHookManager::default())),
            persistent_manager: Arc::new(tokio::sync::Mutex::new(PersistentHookManager::default())),
            call_fn: None,
            on_tool_round: None,
            on_text_response: None,
            initial_with_embeddings: true,
            initial_resume_count: 0,
            max_resume: None,
            pending_async_context: None,
        };

        // Bridge cancel_notify → abort_signal for any caller that signals
        // via the notify without setting the signal directly (HarnxAgent::
        // cancel does both, but this keeps the contract resilient).
        let abort_for_listener = abort_signal.clone();
        let cancel_listener = tokio::task::spawn_local(async move {
            cancel_notify.notified().await;
            abort_for_listener.set_ctrlc();
        });

        // Two-stage cancellation:
        //   1. When `abort_signal` fires, give cooperative-cancel layers
        //      (e.g. AcpManager.session_prompt_with_abort) a grace
        //      window to dispatch `session/cancel` down to any
        //      sub-agents. They poll abort every ~25 ms and then send
        //      a JSON-RPC cancel notification — fast, but not free.
        //   2. After the grace window, hard-cancel `run_agent_loop` by
        //      losing the select! race. This drops any stuck SSE/TCP
        //      reads or stuck tool dispatchers that don't observe
        //      abort — so a hung upstream can't pin the prompt.
        // Pure hard-cancel-on-notify (the previous approach) skipped
        // step 1 — sub-agents were leaked because the AcpManager call
        // was dropped before it could dispatch `session/cancel`.
        // 250 ms is well above the ~30 ms a single AcpManager
        // observes-abort + dispatches-cancel takes; nested layers each
        // run their own grace in parallel, so the bound doesn't
        // compound across depth.
        let abort_for_grace = abort_signal.clone();
        let grace_cancel = async move {
            harnx_core::abort::wait_abort_signal(&abort_for_grace).await;
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        };

        // Scope the per-prompt event sink to this prompt's task tree so
        // concurrent prompts to the same agent never clobber each other's
        // streaming output. The scope wraps the whole select! (including the
        // grace-cancel arm) so any chunks emitted up to the cancellation
        // point still route to this prompt's sink. `loop_result` is
        // `Option<Result<()>>`: `None` means the grace-cancel arm won, a
        // `Some` carries the agent loop's own result.
        // Clone the sink for the remote driver before `sink` is moved into the
        // scoped-sink wrapper used by the local path.
        let remote_sink = sink.clone();
        let loop_result = harnx_core::sink::with_agent_event_sink(sink, async {
            match &remote_agent {
                // Remote thin-client mode (P4.2): drive the turn over NATS via a
                // worker daemon. Events render through the same `remote_sink`
                // (an AcpChunkSink), so chunks reach the parent identically.
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
                // Local mode: run the agent loop in-process.
                None => {
                    // Box the agent-loop future before racing it. `run_agent_loop`
                    // produces a very large future; embedding it inline in `select!`
                    // (which itself lives inside the prompt future + LocalSet frames)
                    // can overflow the thread stack before the first poll. Pinning it
                    // on the heap keeps the synchronous stack frame small.
                    let input = input.expect("local mode always builds input");
                    let mut run_loop = Box::pin(harnx_runtime::run_agent_loop(&loop_ctx, input));
                    tokio::select! {
                        r = &mut run_loop => Some(r),
                        _ = grace_cancel => None,
                    }
                }
            }
        })
        .await;

        let Some(loop_result) = loop_result else {
            cancel_listener.abort();
            fwd_task.abort();
            return Ok(PromptResponse::new(StopReason::Cancelled));
        };

        cancel_listener.abort();
        // Drop loop_ctx so all senders into chunk_rx are dropped and
        // fwd_task can exit cleanly.
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
        session.abort_signal.set_ctrlc();
        session.cancel_notify.notify_one();
        Ok(())
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
