//! harnx-acp-server — ACP server front-end (HarnxAgent) extracted from
//! `harnx::acp::server` (plan P48, β+ progressive peel). Binds the ACP
//! protocol (from `harnx-acp`) to harnx-runtime's Config/Input/Client/tool
//! types.

#[macro_use]
extern crate log;

mod server_main;

pub use server_main::run;

#[cfg(test)]
mod test_regression_issue_68;

use agent_client_protocol::{self as acp, Client as AcpClientTrait};
use harnx_hooks::{AsyncHookManager, PersistentHookManager};
use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc};

use harnx_core::event::{AgentEvent, AgentSource, ModelEvent, ToolEvent};
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
    sessions: RefCell<HashMap<String, HarnxSession>>,
    connection: RefCell<Option<Rc<acp::AgentSideConnection>>>,
}

#[derive(Clone)]
struct HarnxSession {
    #[allow(dead_code)]
    id: String,
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
}

impl HarnxAgent {
    pub fn new(agent_name: String, config: GlobalConfig) -> Self {
        Self {
            agent_name,
            config,
            sessions: RefCell::new(HashMap::new()),
            connection: RefCell::new(None),
        }
    }

    pub fn set_connection(&self, conn: Rc<acp::AgentSideConnection>) {
        self.connection.replace(Some(conn));
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for HarnxAgent {
    async fn initialize(
        &self,
        args: acp::InitializeRequest,
    ) -> acp::Result<acp::InitializeResponse> {
        Ok(acp::InitializeResponse::new(args.protocol_version)
            .agent_capabilities(acp::AgentCapabilities::new())
            .agent_info(
                acp::Implementation::new("harnx", env!("CARGO_PKG_VERSION"))
                    .title(self.agent_name.clone()),
            ))
    }

    async fn authenticate(
        &self,
        _args: acp::AuthenticateRequest,
    ) -> acp::Result<acp::AuthenticateResponse> {
        Ok(acp::AuthenticateResponse::default())
    }

    async fn new_session(
        &self,
        _args: acp::NewSessionRequest,
    ) -> acp::Result<acp::NewSessionResponse> {
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
        }
        let session = HarnxSession {
            id: session_id.clone(),
            abort_signal: AbortSignalInner::new(),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
        };
        self.sessions
            .borrow_mut()
            .insert(session_id.clone(), session);
        Ok(acp::NewSessionResponse::new(acp::SessionId::new(
            session_id,
        )))
    }

    async fn prompt(&self, args: acp::PromptRequest) -> acp::Result<acp::PromptResponse> {
        let session_key = args.session_id.0.to_string();
        let prompt_text: String = args
            .prompt
            .iter()
            .map(content_block_to_text)
            .collect::<Vec<_>>()
            .join("\n");

        let (abort_signal, cancel_notify) = {
            let sessions = self.sessions.borrow();
            let session = sessions
                .get(session_key.as_str())
                .ok_or_else(acp::Error::invalid_params)?;
            session.abort_signal.reset();
            (session.abort_signal.clone(), session.cancel_notify.clone())
        };

        {
            let mut config = self.config.write();
            let active_session_name = config.session.as_ref().map(|s| s.id().to_string());
            if active_session_name.as_deref() != Some(session_key.as_str()) {
                if config.session.is_some() {
                    config.exit_session().map_err(|e| {
                        acp::Error::new(-32603, format!("Failed to exit session: {e}"))
                    })?;
                }
                config
                    .use_agent_by_name(&self.agent_name)
                    .map_err(|e| acp::Error::new(-32603, format!("Failed to set agent: {e}")))?;
                config
                    .use_session(Some(&session_key))
                    .map_err(|e| acp::Error::new(-32603, format!("Failed to use session: {e}")))?;
            }
        }

        // Build a fresh agent for the input.  The agent is also stored on
        // the config (via `use_agent_by_name` above) which is what carries
        // session/shared variables; this local copy is used to expand system
        // prompt variables like {{__os__}} via `set_agent`.
        let mut agent = self
            .config
            .read()
            .retrieve_agent(&self.agent_name)
            .map_err(|e| acp::Error::new(-32603, format!("Failed to retrieve agent: {e}")))?;
        if let Err(e) = harnx_runtime::config::agent::resolve_variables(&mut agent) {
            warn!(
                "Failed to resolve variables for agent '{}': {e}",
                self.agent_name
            );
        }

        let mut input = harnx_runtime::config::input::from_str(&self.config, &prompt_text, None);
        harnx_runtime::config::input::set_agent(&mut input, &self.config, agent.into_config());

        // Install an AgentEventSink for streaming chunks (MessageChunk events)
        // and tool calls (ToolEvent::Started). Nested ACP activity from
        // sub-agent delegations also flows through this sink because
        // `AcpManager::call_tool` registers a forwarder that re-emits each
        // nested chunk via `emit_agent_event_with_source` — the global sink
        // is the single point that converts all events to ACP notifications.
        let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel::<AcpForward>();
        let sink: Arc<dyn harnx_core::event::AgentEventSink> =
            Arc::new(AcpChunkSink { tx: chunk_tx });
        harnx_core::sink::install_agent_event_sink(sink);

        // Spawn local task to drain chunk_rx → session_notification.
        let connection_for_fwd = self.connection.borrow().clone();
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
        fn meta_from_source(
            source: &AgentSource,
        ) -> Option<serde_json::Map<String, serde_json::Value>> {
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
            Some(map)
        }

        fn spawn_notify_text(
            conn: &Option<Rc<acp::AgentSideConnection>>,
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
                    let mut chunk = acp::ContentChunk::new(text.into());
                    if let Some(source) = source.as_ref() {
                        if let Some(meta) = meta_from_source(source) {
                            chunk = chunk.meta(meta);
                        }
                    }
                    let notification = acp::SessionNotification::new(
                        acp::SessionId::new(sid),
                        acp::SessionUpdate::AgentMessageChunk(chunk),
                    );
                    let _ = conn.session_notification(notification).await;
                });
            }
        }

        fn spawn_notify_tool_call(
            conn: &Option<Rc<acp::AgentSideConnection>>,
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
                    let mut tc = acp::ToolCall::new(tool_call_id, name).raw_input(input);
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
                    let notification = acp::SessionNotification::new(
                        acp::SessionId::new(sid),
                        acp::SessionUpdate::ToolCall(tc),
                    );
                    let _ = conn.session_notification(notification).await;
                });
            }
        }

        fn spawn_notify_tool_update(
            conn: &Option<Rc<acp::AgentSideConnection>>,
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
                        harnx_core::event::ToolStatus::Pending => acp::ToolCallStatus::Pending,
                        harnx_core::event::ToolStatus::InProgress => {
                            acp::ToolCallStatus::InProgress
                        }
                        harnx_core::event::ToolStatus::Completed => acp::ToolCallStatus::Completed,
                        harnx_core::event::ToolStatus::Failed => acp::ToolCallStatus::Failed,
                    });
                    let mut fields = acp::ToolCallUpdateFields::new();
                    if let Some(s) = acp_status {
                        fields = fields.status(s);
                    }
                    if let Some(md) = markdown.filter(|t| !t.is_empty()) {
                        fields = fields.title(md);
                    }
                    let mut tcu = acp::ToolCallUpdate::new(id, fields);
                    if let Some(source) = source.as_ref() {
                        if let Some(meta) = meta_from_source(source) {
                            tcu = tcu.meta(meta);
                        }
                    }
                    let notification = acp::SessionNotification::new(
                        acp::SessionId::new(sid),
                        acp::SessionUpdate::ToolCallUpdate(tcu),
                    );
                    let _ = conn.session_notification(notification).await;
                });
            }
        }

        fn spawn_notify_tool_completed(
            conn: &Option<Rc<acp::AgentSideConnection>>,
            session_key: &str,
            id: String,
            output: serde_json::Value,
            markdown: Option<String>,
            source: Option<AgentSource>,
        ) {
            if let Some(conn) = conn.clone() {
                let sid = session_key.to_string();
                tokio::task::spawn_local(async move {
                    let mut fields = acp::ToolCallUpdateFields::new()
                        .status(acp::ToolCallStatus::Completed)
                        .raw_output(output);
                    if let Some(md) = markdown.filter(|t| !t.is_empty()) {
                        fields = fields.title(md);
                    }
                    let mut tcu = acp::ToolCallUpdate::new(id, fields);
                    if let Some(source) = source.as_ref() {
                        if let Some(meta) = meta_from_source(source) {
                            tcu = tcu.meta(meta);
                        }
                    }
                    let notification = acp::SessionNotification::new(
                        acp::SessionId::new(sid),
                        acp::SessionUpdate::ToolCallUpdate(tcu),
                    );
                    let _ = conn.session_notification(notification).await;
                });
            }
        }

        fn spawn_notify_forward(
            conn: &Option<Rc<acp::AgentSideConnection>>,
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
            config: self.config.clone(),
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
        // 100 ms is well above the ~30 ms a single AcpManager
        // observes-abort + dispatches-cancel takes; nested layers each
        // run their own grace in parallel, so the bound doesn't
        // compound across depth.
        let abort_for_grace = abort_signal.clone();
        let grace_cancel = async move {
            harnx_core::abort::wait_abort_signal(&abort_for_grace).await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };

        let loop_result = tokio::select! {
            r = harnx_runtime::run_agent_loop(&loop_ctx, input) => r,
            _ = grace_cancel => {
                cancel_listener.abort();
                harnx_core::sink::clear_agent_event_sink();
                fwd_task.abort();
                return Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
            }
        };

        cancel_listener.abort();
        harnx_core::sink::clear_agent_event_sink();
        // Drop loop_ctx so all senders into chunk_rx are dropped and
        // fwd_task can exit cleanly.
        drop(loop_ctx);
        let _ = fwd_task.await;

        match loop_result {
            Ok(()) => Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            Err(_e) if abort_signal.aborted() => {
                Ok(acp::PromptResponse::new(acp::StopReason::Cancelled))
            }
            Err(e) => Err(acp::Error::new(-32603, format!("Agent loop error: {e:#}"))),
        }
    }

    async fn cancel(&self, args: acp::CancelNotification) -> acp::Result<()> {
        let session_id = args.session_id.0;
        let sessions = self.sessions.borrow();
        let session = sessions
            .get(session_id.as_ref())
            .ok_or_else(acp::Error::invalid_params)?;
        session.abort_signal.set_ctrlc();
        session.cancel_notify.notify_one();
        Ok(())
    }
}

fn content_block_to_text(content: &acp::ContentBlock) -> String {
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
    use agent_client_protocol::Agent;
    use harnx_runtime::{
        client::{ClientConfig, ModelType, TestStateGuard},
        config::{Config, CREATE_TITLE_AGENT},
        test_utils::{MockClient, MockTurnBuilder},
    };
    use std::{
        cell::RefCell,
        pin::Pin,
        rc::Rc,
        sync::Arc,
        task::{Context as TaskContext, Poll},
    };
    use tempfile::TempDir;
    use tokio::io::{AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, ReadBuf};
    use tokio::task::LocalSet;
    use tokio::time::{timeout, Duration};

    struct TokioCompat<T> {
        inner: T,
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

        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_close(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    #[derive(Clone)]
    struct TestClient {
        chunks: Rc<RefCell<Vec<String>>>,
    }

    #[async_trait::async_trait(?Send)]
    impl acp::Client for TestClient {
        async fn request_permission(
            &self,
            _args: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            Err(acp::Error::method_not_found())
        }

        async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
            if let acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk { content, .. }) =
                args.update
            {
                let text = content_block_to_text(&content);
                if !text.is_empty() {
                    self.chunks.borrow_mut().push(text);
                }
            }
            Ok(())
        }
    }

    fn run_local<F: std::future::Future>(future: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build ACP server test runtime");
        let local_set = LocalSet::new();
        local_set.block_on(&rt, future)
    }

    #[test]
    fn acp_chunk_sink_forwards_tool_completed_and_update_to_channel() {
        use harnx_core::event::{AgentEvent, AgentEventSink, ToolEvent, ToolStatus};
        use tokio::sync::mpsc::unbounded_channel;

        let (tx, mut rx) = unbounded_channel::<AcpForward>();
        let sink = AcpChunkSink { tx };

        sink.emit(
            AgentEvent::Tool(ToolEvent::Completed {
                id: "call-1".to_string(),
                output: serde_json::json!({"text": "result"}),
                markdown: Some("**result**".to_string()),
            }),
            None,
        );

        sink.emit(
            AgentEvent::Tool(ToolEvent::Update {
                id: "call-1".to_string(),
                markdown: None,
                status: Some(ToolStatus::InProgress),
                content: None,
            }),
            None,
        );

        let completed = rx.try_recv().expect("should have ToolCompleted");
        let update = rx.try_recv().expect("should have ToolUpdate");

        match completed {
            AcpForward::ToolCompleted {
                id,
                output,
                markdown,
                ..
            } => {
                assert_eq!(id, "call-1");
                assert_eq!(output, serde_json::json!({"text": "result"}));
                assert_eq!(markdown.as_deref(), Some("**result**"));
            }
            _ => panic!("expected ToolCompleted"),
        }
        match update {
            AcpForward::ToolUpdate { id, status, .. } => {
                assert_eq!(id, "call-1");
                assert!(matches!(status, Some(ToolStatus::InProgress)));
            }
            _ => panic!("expected ToolUpdate"),
        }
    }

    fn test_config() -> GlobalConfig {
        use parking_lot::RwLock;
        use std::sync::Arc;

        let clients: Vec<ClientConfig> = serde_yaml::from_str(
            r#"
- type: openai
  api_key: test-key
  models:
    - name: gpt-4o
      type: chat
      max_input_tokens: 128000
      max_output_tokens: 8192
"#,
        )
        .expect("parse test client config");

        let mut config = Config::default();
        config.clients = clients;
        config.model = harnx_runtime::client::retrieve_model(
            &config.clients,
            "openai:gpt-4o",
            ModelType::Chat,
        )
        .expect("load test model");
        config.save_session = Some(true);

        Arc::new(RwLock::new(config))
    }

    fn write_test_agent(temp: &TempDir, agent_name: &str, prompt: &str) {
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).expect("create agents dir");
        let agent_content = format!("---\nmodel: openai:gpt-4o\n---\n{prompt}\n");
        std::fs::write(agents_dir.join(format!("{agent_name}.md")), agent_content)
            .expect("write test agent file");
    }

    struct EnvGuard(&'static str, Option<std::ffi::OsString>);

    impl EnvGuard {
        fn new(key: &'static str, value: &std::path::Path) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self(key, prev)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(v) = self.1.take() {
                unsafe { std::env::set_var(self.0, v) };
            } else {
                unsafe { std::env::remove_var(self.0) };
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn setup_roundtrip(
        agent_name: &str,
        config: GlobalConfig,
    ) -> (
        acp::ClientSideConnection,
        Rc<RefCell<Vec<String>>>,
        tokio::task::JoinHandle<acp::Result<()>>,
        tokio::task::JoinHandle<acp::Result<()>>,
    ) {
        let agent = Rc::new(HarnxAgent::new(agent_name.to_string(), config));
        let (server_stream, client_stream) = tokio::io::duplex(16 * 1024);
        let (server_reader, server_writer) = tokio::io::split(server_stream);
        let (client_reader, client_writer) = tokio::io::split(client_stream);

        let (server_conn, server_io) = acp::AgentSideConnection::new(
            Rc::clone(&agent),
            TokioCompat::new(server_writer),
            TokioCompat::new(server_reader),
            |future| {
                tokio::task::spawn_local(future);
            },
        );
        agent.set_connection(Rc::new(server_conn));

        let chunks = Rc::new(RefCell::new(Vec::new()));
        let (client_conn, client_io) = acp::ClientSideConnection::new(
            TestClient {
                chunks: Rc::clone(&chunks),
            },
            TokioCompat::new(client_writer),
            TokioCompat::new(client_reader),
            |future| {
                tokio::task::spawn_local(future);
            },
        );

        let server_handle = tokio::task::spawn_local(server_io);
        let client_handle = tokio::task::spawn_local(client_io);

        (client_conn, chunks, server_handle, client_handle)
    }

    #[test]
    fn test_new_session_returns_unique_ids() {
        let temp = tempfile::tempdir().expect("create temp dir");
        write_test_agent(&temp, "test", "You are test agent.");
        let config = test_config();
        let temp_path = temp.path().to_path_buf();
        run_local(async move {
            let _guard = TestStateGuard::new(None).await;
            let _env = EnvGuard::new("HARNX_CONFIG_DIR", &temp_path);
            let agent = HarnxAgent::new("test".to_string(), config);
            let cwd = std::env::current_dir().expect("current dir");

            let resp1 = agent
                .new_session(acp::NewSessionRequest::new(cwd.clone()))
                .await
                .expect("create first session");
            let resp2 = agent
                .new_session(acp::NewSessionRequest::new(cwd))
                .await
                .expect("create second session");
            let session_id1 = resp1.session_id.0.to_string();
            let session_id2 = resp2.session_id.0.to_string();

            assert_ne!(resp1.session_id, resp2.session_id);
            assert!(agent.sessions.borrow().contains_key(session_id1.as_str()));
            assert!(agent.sessions.borrow().contains_key(session_id2.as_str()));
        });
    }

    #[test]
    fn test_cancel_marks_session() {
        let temp = tempfile::tempdir().expect("create temp dir");
        write_test_agent(&temp, "test", "You are test agent.");
        let config = test_config();
        let temp_path = temp.path().to_path_buf();
        run_local(async move {
            let _guard = TestStateGuard::new(None).await;
            let _env = EnvGuard::new("HARNX_CONFIG_DIR", &temp_path);
            let agent = HarnxAgent::new("test".to_string(), config);
            let response = agent
                .new_session(acp::NewSessionRequest::new(
                    std::env::current_dir().expect("current dir"),
                ))
                .await
                .expect("create session");
            let session_id = response.session_id.0.to_string();

            agent
                .cancel(acp::CancelNotification::new(session_id.clone()))
                .await
                .expect("cancel session");

            let sessions = agent.sessions.borrow();
            let session = sessions.get(session_id.as_str()).expect("stored session");
            assert!(session.abort_signal.aborted());
        });
    }

    #[test]
    fn test_cancel_unknown_session_errors() {
        let config = test_config();
        run_local(async move {
            let agent = HarnxAgent::new("test".to_string(), config);

            let result = agent
                .cancel(acp::CancelNotification::new("nonexistent".to_string()))
                .await;

            assert!(result.is_err());
        });
    }

    #[test]
    fn test_acp_server_initialize_handshake() {
        let config = test_config();

        run_local(async move {
            let (client_conn, _chunks, server_handle, client_handle) =
                setup_roundtrip("test", config);

            let response = timeout(
                Duration::from_secs(5),
                client_conn.initialize(
                    acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_info(
                        acp::Implementation::new("test-client", "0.1.0").title("Test Client"),
                    ),
                ),
            )
            .await
            .expect("initialize should not hang")
            .expect("initialize should succeed");

            assert_eq!(response.protocol_version, acp::ProtocolVersion::V1);
            assert!(response.agent_info.is_some());

            server_handle.abort();
            client_handle.abort();
        });
    }

    #[test]
    fn test_acp_server_new_session_and_prompt_roundtrip() {
        let temp = tempfile::tempdir().expect("create temp dir");
        write_test_agent(
            &temp,
            CREATE_TITLE_AGENT,
            "You create concise titles for conversations.",
        );
        let config = test_config();
        let temp_path = temp.path().to_path_buf();

        run_local(async move {
            let _guard = TestStateGuard::new(Some(Arc::new(
                MockClient::builder()
                    .add_turn(
                        MockTurnBuilder::new()
                            .add_text_chunk("mock roundtrip response")
                            .build(),
                    )
                    .build(),
            )))
            .await;
            let _env = EnvGuard::new("HARNX_CONFIG_DIR", &temp_path);

            let (client_conn, chunks, server_handle, client_handle) =
                setup_roundtrip(CREATE_TITLE_AGENT, config.clone());

            timeout(
                Duration::from_secs(5),
                client_conn.initialize(
                    acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_info(
                        acp::Implementation::new("test-client", "0.1.0").title("Test Client"),
                    ),
                ),
            )
            .await
            .expect("initialize should not hang")
            .expect("initialize should succeed");

            let session = timeout(
                Duration::from_secs(5),
                client_conn.new_session(acp::NewSessionRequest::new(
                    std::env::current_dir().expect("current dir"),
                )),
            )
            .await
            .expect("new_session should not hang")
            .expect("new_session should succeed");

            assert_eq!(
                config.read().session.as_ref().map(|s| s.id().to_string()),
                Some(session.session_id.to_string())
            );

            let response = timeout(
                Duration::from_secs(5),
                client_conn.prompt(acp::PromptRequest::new(
                    session.session_id.to_string(),
                    vec![acp::ContentBlock::from("hello from client")],
                )),
            )
            .await
            .expect("prompt should not hang")
            .expect("prompt should succeed");

            assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
            let chunks = chunks.borrow();
            assert!(
                chunks.iter().any(|chunk| !chunk.trim().is_empty()),
                "expected prompt roundtrip output to include at least one non-empty chunk, got {:?}",
                *chunks
            );

            let session_id = session.session_id.to_string();
            let session_path =
                harnx_core::config_paths::session_file(Some(CREATE_TITLE_AGENT), &session_id);
            let top_level_path = harnx_core::config_paths::session_file(None, &session_id);
            assert!(
                session_path.exists(),
                "ACP prompt should persist session to disk at {}",
                session_path.display()
            );
            assert!(
                !top_level_path.exists(),
                "session must NOT be saved to top-level path {}, should be agent-scoped",
                top_level_path.display()
            );

            // Verify session file actually contains conversation content.
            let session_content =
                std::fs::read_to_string(&session_path).expect("read session file");
            assert!(
                session_content.contains("hello from client"),
                "session file should contain the user prompt; got:\n{session_content}"
            );
            assert!(
                session_content.contains("mock roundtrip response"),
                "session file should contain the assistant response; got:\n{session_content}"
            );

            server_handle.abort();
            client_handle.abort();
        });
    }

    /// Verify that system-level variables (e.g. `{{__os__}}`) are expanded in
    /// the agent's system prompt when running in ACP mode.  The session file
    /// should contain the expanded OS name, not the raw `{{__os__}}` template.
    #[test]
    fn test_acp_prompt_expands_system_variables_in_session() {
        let temp = tempfile::tempdir().expect("create temp dir");
        write_test_agent(
            &temp,
            "vartest",
            "You are running on {{__os__}}. Help the user.",
        );

        let config = test_config();
        let temp_path = temp.path().to_path_buf();

        run_local(async move {
            let _guard = TestStateGuard::new(Some(Arc::new(
                MockClient::builder()
                    .add_turn(
                        MockTurnBuilder::new()
                            .add_text_chunk("variable expansion response")
                            .build(),
                    )
                    .build(),
            )))
            .await;
            let _env = EnvGuard::new("HARNX_CONFIG_DIR", &temp_path);

            let (client_conn, _chunks, server_handle, client_handle) =
                setup_roundtrip("vartest", config.clone());

            timeout(
                Duration::from_secs(5),
                client_conn.initialize(
                    acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_info(
                        acp::Implementation::new("test-client", "0.1.0").title("Test Client"),
                    ),
                ),
            )
            .await
            .expect("initialize should not hang")
            .expect("initialize should succeed");

            let session = timeout(
                Duration::from_secs(5),
                client_conn.new_session(acp::NewSessionRequest::new(
                    std::env::current_dir().expect("current dir"),
                )),
            )
            .await
            .expect("new_session should not hang")
            .expect("new_session should succeed");

            let _response = timeout(
                Duration::from_secs(5),
                client_conn.prompt(acp::PromptRequest::new(
                    session.session_id.to_string(),
                    vec![acp::ContentBlock::from("expand variables test")],
                )),
            )
            .await
            .expect("prompt should not hang")
            .expect("prompt should succeed");

            // The session file should contain the expanded OS name
            // (e.g. "linux", "macos") instead of the raw template.
            let session_id = session.session_id.to_string();
            let session_path = harnx_core::config_paths::session_file(Some("vartest"), &session_id);
            let top_level_path = harnx_core::config_paths::session_file(None, &session_id);
            assert!(
                session_path.exists(),
                "session file should exist at {}",
                session_path.display()
            );
            assert!(
                !top_level_path.exists(),
                "session must NOT be saved to top-level path {}, should be agent-scoped",
                top_level_path.display()
            );
            let content = std::fs::read_to_string(&session_path).expect("read session");
            assert!(
                !content.contains("{{__os__}}"),
                "session should not contain unexpanded {{{{__os__}}}} variable; got:\n{content}"
            );
            let current_os = std::env::consts::OS;
            assert!(
                content.contains(current_os),
                "session should contain expanded OS name '{current_os}'; got:\n{content}"
            );

            server_handle.abort();
            client_handle.abort();
        });
    }
}
