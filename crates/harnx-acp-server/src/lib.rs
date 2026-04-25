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
use harnx_acp::NestedAcpEvent;
use harnx_hooks::{AsyncHookManager, PersistentHookManager};
use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc};
use uuid::Uuid;

use harnx_core::event::{AgentEvent, AgentSource, ModelEvent};
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::utils::{AbortSignal, AbortSignalInner};

/// An `AgentEventSink` installed during each ACP prompt turn.
/// Forwards `MessageChunk` events from the unified run_agent_loop through a
/// channel to a spawned local task that calls `session_notification`.
/// Using a channel avoids the `!Send` / `!Sync` problem of holding `Rc`.
struct AcpChunkSink {
    tx: tokio::sync::mpsc::UnboundedSender<String>,
}

impl harnx_core::event::AgentEventSink for AcpChunkSink {
    fn emit(&self, event: AgentEvent, source: Option<AgentSource>) {
        if let Some(source) = source.as_ref() {
            let _ = self.tx.send(source_heading(source));
        }

        let text = match &event {
            AgentEvent::Model(ModelEvent::MessageChunk { blocks }) => blocks
                .iter()
                .filter_map(|b| match b {
                    harnx_core::event::ContentBlock::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<String>(),
            AgentEvent::Model(ModelEvent::Final { output, .. }) => output.clone(),
            _ => return,
        };
        if !text.is_empty() {
            let _ = self.tx.send(text);
        }
    }
}

fn source_heading(source: &AgentSource) -> String {
    match &source.session_id {
        Some(session_id) if !session_id.is_empty() => {
            format!("> {} ▸ {}", source.agent, session_id)
        }
        _ => format!("> {}", source.agent),
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
        let session_id = Uuid::new_v4().to_string();
        {
            let mut config = self.config.write();
            if config.session.is_some() {
                config
                    .exit_session()
                    .map_err(|e| acp::Error::new(-32603, format!("Failed to exit session: {e}")))?;
            }
            config
                .use_session(Some(&session_id))
                .map_err(|e| acp::Error::new(-32603, format!("Failed to create session: {e}")))?;
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
            let active_session_name = config.session.as_ref().map(|s| s.name().to_string());
            if active_session_name.as_deref() != Some(session_key.as_str()) {
                if config.session.is_some() {
                    config.exit_session().map_err(|e| {
                        acp::Error::new(-32603, format!("Failed to exit session: {e}"))
                    })?;
                }
                config
                    .use_session(Some(&session_key))
                    .map_err(|e| acp::Error::new(-32603, format!("Failed to use session: {e}")))?;
            }
        }

        // Load and resolve the agent (expands system prompt variables like
        // {{__os__}}). In non-ACP flows this happens via
        // init_agent_session_variables; in ACP mode we do it here since the
        // agent is not stored on the config.
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

        // Install an AgentEventSink for streaming chunks (MessageChunk events).
        // For non-streaming turns, on_text_response sends full text below.
        // Nested ACP activity must also be subscribed + forwarded.
        let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let text_tx = chunk_tx.clone();
        let sink: Arc<dyn harnx_core::event::AgentEventSink> =
            Arc::new(AcpChunkSink { tx: chunk_tx });
        harnx_core::sink::install_agent_event_sink(sink);

        let acp_manager = self.config.read().acp_manager.clone();
        let (mut nested_rx, nested_sub_id) = if let Some(ref mgr) = acp_manager {
            let (rx, id) = mgr.subscribe_chunks().await;
            (Some(rx), Some(id))
        } else {
            (None, None)
        };

        // Spawn local task to drain both local loop chunks and nested ACP chunks.
        let connection_for_fwd = self.connection.borrow().clone();
        let session_key_for_fwd = session_key.clone();
        // Helper: fire a session_notification without blocking the LocalSet
        // thread. Each notification is spawned as its own local task so
        // run_agent_loop / execute_tool_round are never starved waiting for
        // the parent to acknowledge a notification write.
        fn spawn_notify(
            conn: &Option<Rc<acp::AgentSideConnection>>,
            session_key: &str,
            text: String,
        ) {
            if text.is_empty() {
                return;
            }
            if let Some(conn) = conn.clone() {
                let sid = session_key.to_string();
                tokio::task::spawn_local(async move {
                    let notification = acp::SessionNotification::new(
                        acp::SessionId::new(sid),
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(text.into())),
                    );
                    let _ = conn.session_notification(notification).await;
                });
            }
        }

        let fwd_task = tokio::task::spawn_local(async move {
            // Phase 1: drain both chunk_rx and nested_rx concurrently.
            // Each notification is fire-and-forget (spawn_local) so this loop
            // never blocks waiting for the parent to ACK a write. This keeps
            // run_agent_loop / execute_tool_round schedulable on the same
            // LocalSet thread.
            'outer: loop {
                let Some(ref mut nrx) = nested_rx else {
                    break 'outer;
                };
                tokio::select! {
                    maybe_text = chunk_rx.recv() => match maybe_text {
                        Some(text) => {
                            spawn_notify(&connection_for_fwd, &session_key_for_fwd, text);
                        }
                        None => break 'outer,
                    },
                    maybe_nested = nrx.recv() => match maybe_nested {
                        Some(NestedAcpEvent::Text(text)) => {
                            spawn_notify(&connection_for_fwd, &session_key_for_fwd, text);
                        }
                        Some(NestedAcpEvent::Agent(event, source)) => {
                            if let Some(source) = source.as_ref() {
                                spawn_notify(&connection_for_fwd, &session_key_for_fwd, source_heading(source));
                            }
                            let text = match event {
                                AgentEvent::Model(ModelEvent::MessageChunk { blocks }) => blocks
                                    .into_iter()
                                    .filter_map(|b| match b {
                                        harnx_core::event::ContentBlock::Text(t) => Some(t),
                                        _ => None,
                                    })
                                    .collect::<String>(),
                                AgentEvent::Model(ModelEvent::Final { output, .. }) => output,
                                AgentEvent::Notice(harnx_core::event::NoticeEvent::Info(text)) => text,
                                AgentEvent::Model(ModelEvent::Error(text)) => text,
                                _ => String::new(),
                            };
                            spawn_notify(&connection_for_fwd, &session_key_for_fwd, text);
                        }
                        None => {
                            nested_rx = None;
                            break 'outer;
                        }
                    }
                }
            }
            // Phase 2: drain remaining chunk_rx items.
            while let Some(text) = chunk_rx.recv().await {
                spawn_notify(&connection_for_fwd, &session_key_for_fwd, text);
            }
        });

        // on_text_response: called by run_agent_loop with the actual output text
        // when the turn ends with a plain-text response (no tool calls). Routes
        // through the same channel as streaming chunks so the fwd_task sends it
        // to the ACP client. This keeps the closure Send (no Rc captured).
        // text_tx was cloned from chunk_tx above
        let on_text_response: harnx_runtime::OnTextResponseFn =
            Arc::new(move |output: String, _usage| {
                let tx = text_tx.clone();
                Box::pin(async move {
                    if !output.is_empty() {
                        let _ = tx.send(output);
                    }
                })
            });

        let loop_ctx = harnx_runtime::AgentLoopContext {
            config: self.config.clone(),
            abort_signal: abort_signal.clone(),
            async_manager: Arc::new(tokio::sync::Mutex::new(AsyncHookManager::default())),
            persistent_manager: Arc::new(tokio::sync::Mutex::new(PersistentHookManager::default())),
            call_fn: None,
            on_tool_round: None,
            on_text_response: Some(on_text_response),
            initial_with_embeddings: true,
            initial_resume_count: 0,
            max_resume: None,
            pending_async_context: None,
        };

        let loop_result = tokio::select! {
            r = harnx_runtime::run_agent_loop(&loop_ctx, input) => r,
            _ = cancel_notify.notified() => {
                abort_signal.set_ctrlc();
                harnx_core::sink::clear_agent_event_sink();
                if let (Some(mgr), Some(id)) = (acp_manager.as_ref(), nested_sub_id) {
                    mgr.unsubscribe_chunks(id).await;
                }
                fwd_task.abort();
                return Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
            }
        };

        harnx_core::sink::clear_agent_event_sink();
        if let (Some(mgr), Some(id)) = (acp_manager.as_ref(), nested_sub_id) {
            mgr.unsubscribe_chunks(id).await;
        }
        // Drop loop_ctx to release text_tx (a clone of chunk_tx), so all
        // senders into chunk_rx are dropped and fwd_task can exit cleanly.
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
        let config = test_config();
        run_local(async move {
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
        let config = test_config();
        run_local(async move {
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
        let config = test_config();

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
                config.read().session.as_ref().map(|s| s.name().to_string()),
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

            let session_path = config.read().session_file(&session.session_id.to_string());
            assert!(
                !session_path.display().to_string().contains("/sessions/_/"),
                "session file should not be written under '_' temp directory: {}",
                session_path.display()
            );
            assert!(
                session_path.exists(),
                "ACP prompt should persist the session to disk at {}",
                session_path.display()
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
        let temp = tempfile::TempDir::new().unwrap();
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();

        // Write an agent whose prompt contains {{__os__}}.
        let agent_content =
            "---\nmodel: openai:gpt-4o\n---\nYou are running on {{__os__}}. Help the user.\n";
        std::fs::write(agents_dir.join("vartest.md"), agent_content).unwrap();

        // Point HARNX_CONFIG_DIR at the temp dir so retrieve_agent finds it.
        // The env mutation must be serialized with other tests that touch
        // HARNX_CONFIG_DIR — we do this by creating the guard *inside* the
        // region where `TEST_CLIENT_LOCK` is held (via `TestStateGuard`).
        struct EnvGuard(&'static str, Option<std::ffi::OsString>);
        impl EnvGuard {
            fn new(key: &'static str, val: &std::path::Path) -> Self {
                let prev = std::env::var_os(key);
                unsafe { std::env::set_var(key, val) };
                Self(key, prev)
            }
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match &self.1 {
                    Some(v) => unsafe { std::env::set_var(self.0, v) },
                    None => unsafe { std::env::remove_var(self.0) },
                }
            }
        }

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
            let session_path = config.read().session_file(&session.session_id.to_string());
            assert!(
                session_path.exists(),
                "session file should exist at {}",
                session_path.display()
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
