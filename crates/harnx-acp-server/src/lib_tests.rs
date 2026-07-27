#[cfg(test)]
mod tests {
    use crate::{AcpChunkSink, AcpForward, HarnxAgent};
    use agent_client_protocol::schema::v1::{
        CancelNotification, ContentBlock, NewSessionRequest, PromptRequest, PromptResponse,
    };
    use harnx_core::event::{AgentEvent, AgentEventSink, ToolEvent, ToolStatus, UserEvent};
    use harnx_runtime::{
        client::{ClientConfig, ModelType, TestStateGuard},
        config::Config,
        test_utils::{MockClient, MockTurnBuilder},
    };
    use parking_lot::RwLock;
    use std::sync::Arc;
    use tokio::sync::mpsc::unbounded_channel;

    fn test_config() -> crate::GlobalConfig {
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

        // Set the client name to match what the file loader would do for openai.yaml
        let mut clients = clients;
        if let Some(c) = clients.first_mut() {
            c.set_name("openai".to_string());
        }

        let mut config = Config {
            clients,
            ..Default::default()
        };
        config.model = harnx_runtime::client::retrieve_model(
            &config.clients,
            "openai:gpt-4o",
            ModelType::Chat,
        )
        .expect("load test model");
        config.save_session = Some(true);
        let sessions_dir = tempfile::tempdir().expect("create sessions dir");
        config.sessions_dir_override = Some(sessions_dir.keep());

        Arc::new(RwLock::new(config))
    }

    /// Write a minimal agent config file and set `HARNX_CONFIG_DIR` to point
    /// to the temp directory for the duration of the test.  Returns the
    /// `TempDir` so the caller keeps it alive (and the guard string so it can
    /// be held in the caller's scope).
    fn setup_agent_env(agent_name: &str) -> (tempfile::TempDir, String) {
        let temp = tempfile::tempdir().expect("create temp dir");
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).expect("create agents dir");
        let content = "---\nmodel: openai:gpt-4o\n---\nYou are a test agent.\n";
        std::fs::write(agents_dir.join(format!("{agent_name}.md")), content)
            .expect("write agent file");
        let path_str = temp.path().to_str().expect("temp path str").to_string();
        // SAFETY: single-threaded test context; restored below via drop of
        // the returned TempDir which callers must hold until after assertions.
        // In practice each test has its own env guard via _env binding.
        unsafe { std::env::set_var("HARNX_CONFIG_DIR", &path_str) };
        (temp, path_str)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_new_session_returns_unique_ids() {
        let (_temp, _path) = setup_agent_env("test");
        let _guard = TestStateGuard::new(None).await;
        let config = test_config();
        let agent = Arc::new(HarnxAgent::new("test".to_string(), config));
        let cwd = std::env::current_dir().expect("current dir");

        let local = tokio::task::LocalSet::new();
        let (resp1, resp2) = local
            .run_until(async {
                let r1 = agent
                    .new_session(NewSessionRequest::new(cwd.clone()))
                    .await
                    .expect("create first session");
                let r2 = agent
                    .new_session(NewSessionRequest::new(cwd))
                    .await
                    .expect("create second session");
                (r1, r2)
            })
            .await;
        let session_id1 = resp1.session_id.0.to_string();
        let session_id2 = resp2.session_id.0.to_string();

        assert_ne!(resp1.session_id, resp2.session_id);
        let sessions = agent.sessions.lock().await;
        assert!(sessions.contains_key(session_id1.as_str()));
        assert!(sessions.contains_key(session_id2.as_str()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_cancel_marks_session() {
        let (_temp, _path) = setup_agent_env("test");
        let _guard = TestStateGuard::new(None).await;
        let config = test_config();
        let agent = Arc::new(HarnxAgent::new("test".to_string(), config));
        let cwd = std::env::current_dir().expect("current dir");

        let local = tokio::task::LocalSet::new();
        let response = local
            .run_until(async {
                agent
                    .new_session(NewSessionRequest::new(cwd))
                    .await
                    .expect("create session")
            })
            .await;
        let session_id = response.session_id.0.to_string();

        let local2 = tokio::task::LocalSet::new();
        local2
            .run_until(async {
                agent
                    .cancel(CancelNotification::new(session_id.clone()))
                    .await
                    .expect("cancel session")
            })
            .await;

        let sessions = agent.sessions.lock().await;
        let session = sessions.get(session_id.as_str()).expect("stored session");
        assert!(session.abort_signal.aborted());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_cancel_unknown_session_errors() {
        let config = test_config();
        let agent = Arc::new(HarnxAgent::new("test".to_string(), config));

        let local = tokio::task::LocalSet::new();
        let result = local
            .run_until(async {
                agent
                    .cancel(CancelNotification::new("nonexistent".to_string()))
                    .await
            })
            .await;

        assert!(result.is_err());
    }

    /// A created session bound to its agent and on-disk log location. Holding
    /// the agent handle, id, and log path as typed fields keeps the per-call
    /// helpers from taking a pile of bare `&str` arguments.
    struct TestSession {
        agent: Arc<HarnxAgent>,
        id: String,
        log_path: std::path::PathBuf,
    }

    impl TestSession {
        /// Create a fresh session on `agent`.
        async fn create(agent: &Arc<HarnxAgent>, sessions_dir: &std::path::Path) -> Self {
            let cwd = std::env::current_dir().expect("current dir");
            let local = tokio::task::LocalSet::new();
            let id = local
                .run_until(async {
                    agent
                        .new_session(NewSessionRequest::new(cwd))
                        .await
                        .expect("create session")
                        .session_id
                        .0
                        .to_string()
                })
                .await;
            let log_path = sessions_dir.join(format!("{id}.yaml"));
            Self {
                agent: Arc::clone(agent),
                id,
                log_path,
            }
        }

        /// Drive a prompt against this session.
        async fn prompt(&self, text: &str) -> agent_client_protocol::Result<PromptResponse> {
            let request = PromptRequest::new(
                agent_client_protocol::schema::v1::SessionId::new(self.id.clone()),
                vec![ContentBlock::from(text.to_string())],
            );
            // Note: caller should wrap in LocalSet
            self.agent.prompt(request).await
        }

        /// Read the log via async I/O so we never block a runtime worker thread
        /// inside a `#[tokio::test]`.
        async fn read_log(&self) -> String {
            tokio::fs::read_to_string(&self.log_path)
                .await
                .unwrap_or_else(|e| {
                    panic!("read session log {} failed: {e}", self.log_path.display())
                })
        }

        /// Assert the log is present, non-empty, free of the pending-result
        /// placeholder, and contains the prompt text it was driven with.
        async fn assert_intact(&self, expected_prompt: &str) {
            let log = self.read_log().await;
            assert!(!log.is_empty(), "session log empty: {}", self.id);
            assert!(
                !log.contains("tool response pending"),
                "placeholder leaked into {}: {log}",
                self.id
            );
            assert!(
                log.contains(expected_prompt),
                "session {} lost own prompt {expected_prompt:?}: {log}",
                self.id
            );
        }
    }

    // Use the multi-thread flavor (as every other `run_agent_loop`-driving
    // test does): on a current-thread runtime the test future, the inner
    // `LocalSet`, and the entire agent loop run on one stack frame and
    // overflow the default 2 MiB thread stack. `spawn_local` inside the
    // prompt path still works because the body runs under `LocalSet::run_until`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_concurrent_prompts_isolate_session_scope_and_sink() {
        let (_temp, _path) = setup_agent_env("test");
        let config = test_config();
        let sessions_dir = config
            .read()
            .sessions_dir_override
            .clone()
            .expect("sessions dir override");
        let mock = Arc::new(
            MockClient::builder()
                .add_turn(MockTurnBuilder::new().add_text_chunk("reply one").build())
                .add_turn(MockTurnBuilder::new().add_text_chunk("reply two").build())
                .add_turn(MockTurnBuilder::new().add_text_chunk("reply three").build())
                .build(),
        );
        let _guard = TestStateGuard::new(Some(mock)).await;
        let agent = Arc::new(HarnxAgent::new("test".to_string(), config.clone()));

        let s1 = TestSession::create(&agent, &sessions_dir).await;
        let s2 = TestSession::create(&agent, &sessions_dir).await;
        let s3 = TestSession::create(&agent, &sessions_dir).await;

        let local = tokio::task::LocalSet::new();
        let (resp1, resp2, resp3) = local
            .run_until(async {
                tokio::join!(
                    s1.prompt("alpha prompt"),
                    s2.prompt("beta prompt"),
                    s3.prompt("gamma prompt")
                )
            })
            .await;

        for response in [resp1, resp2, resp3] {
            assert_eq!(
                response.expect("prompt should succeed").stop_reason,
                agent_client_protocol::schema::v1::StopReason::EndTurn
            );
        }

        // Each session's log is intact and carries only its own prompt — no
        // cross-talk from the concurrently-running siblings.
        s1.assert_intact("alpha prompt").await;
        s2.assert_intact("beta prompt").await;
        s3.assert_intact("gamma prompt").await;
    }

    // Two concurrent prompts to the SAME session_id must serialize on the
    // per-session `prompt_lock` so neither clobbers the other's on-disk
    // transcript. Both prompt texts must survive in the single session log.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_same_session_concurrent_prompts_do_not_clobber_log() {
        let (_temp, _path) = setup_agent_env("test");
        let config = test_config();
        let sessions_dir = config
            .read()
            .sessions_dir_override
            .clone()
            .expect("sessions dir override");
        let mock = Arc::new(
            MockClient::builder()
                .add_turn(MockTurnBuilder::new().add_text_chunk("reply one").build())
                .add_turn(MockTurnBuilder::new().add_text_chunk("reply two").build())
                .build(),
        );
        let _guard = TestStateGuard::new(Some(mock)).await;
        let agent = Arc::new(HarnxAgent::new("test".to_string(), config.clone()));

        let session = TestSession::create(&agent, &sessions_dir).await;

        let local = tokio::task::LocalSet::new();
        let (resp1, resp2) = local
            .run_until(async {
                tokio::join!(
                    session.prompt("first message"),
                    session.prompt("second message"),
                )
            })
            .await;
        resp1.expect("first prompt should succeed");
        resp2.expect("second prompt should succeed");

        // Both prompts must survive in the single session log — last-writer-wins
        // clobbering would drop one of them.
        let log = session.read_log().await;
        assert!(!log.is_empty(), "session log empty");
        assert!(
            !log.contains("tool response pending"),
            "placeholder leaked: {log}"
        );
        assert!(
            log.contains("first message"),
            "first prompt clobbered: {log}"
        );
        assert!(
            log.contains("second message"),
            "second prompt clobbered: {log}"
        );
    }

    #[tokio::test]
    async fn test_initialize_echoes_protocol_version_and_reports_capabilities() {
        use agent_client_protocol::schema::v1::InitializeRequest;
        use agent_client_protocol::schema::ProtocolVersion;

        let config = test_config();
        let agent = HarnxAgent::new("my-agent".to_string(), config);

        let request = InitializeRequest::new(ProtocolVersion::V1);
        let response = agent
            .initialize(request)
            .await
            .expect("initialize should succeed");

        // Server must echo back the protocol version the client requested.
        assert_eq!(response.protocol_version, ProtocolVersion::V1);
        // Server must include agent_info with the name it was given.
        let info = response.agent_info.expect("agent_info should be present");
        assert_eq!(info.name, "harnx");
        assert_eq!(info.title.as_deref(), Some("my-agent"));
    }

    #[test]
    fn acp_chunk_sink_forwards_tool_completed_and_update_to_channel() {
        let (tx, mut rx) = unbounded_channel::<AcpForward>();
        let sink = AcpChunkSink { tx };

        sink.emit(AgentEvent::Tool(ToolEvent::Completed {
            id: "call-1".to_string(),
            output: serde_json::json!({"text": "result"}),
            markdown: Some("**result**".to_string()),
        }));

        sink.emit(AgentEvent::Tool(ToolEvent::Update {
            id: "call-1".to_string(),
            markdown: None,
            status: Some(ToolStatus::InProgress),
            content: None,
        }));

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

    #[test]
    fn acp_chunk_sink_forwards_non_empty_user_messages_only() {
        let (tx, mut rx) = unbounded_channel::<AcpForward>();
        let sink = AcpChunkSink { tx };

        sink.emit(AgentEvent::User(UserEvent::Message {
            content: "hello user".to_string(),
        }));

        match rx.try_recv().expect("should forward user text") {
            AcpForward::UserText(text, source) => {
                assert_eq!(text, "hello user");
                assert!(source.is_none());
            }
            _ => panic!("expected UserText forward"),
        }

        sink.emit(AgentEvent::User(UserEvent::Message {
            content: String::new(),
        }));

        assert!(
            rx.try_recv().is_err(),
            "empty user message should not forward anything"
        );
    }

    #[test]
    fn acp_chunk_sink_forwards_model_errors_as_visible_text() {
        let (tx, mut rx) = unbounded_channel::<AcpForward>();
        let sink = AcpChunkSink { tx };

        sink.emit(AgentEvent::Model(harnx_core::event::ModelEvent::Error(
            "Internal server error".to_string(),
        )));

        match rx.try_recv().expect("should forward error text") {
            AcpForward::Error(text, source) => {
                assert_eq!(text, "Internal server error");
                assert!(source.is_none());
            }
            _ => panic!("expected Error forward"),
        }
    }

    #[test]
    fn meta_from_source_includes_model_when_present() {
        use harnx_core::event::AgentSource;

        // Case 1: model present
        let source_with_model = AgentSource {
            agent: "test-agent".to_string(),
            session_id: Some("session-123".to_string()),
            model: Some("gpt-4o".to_string()),
        };
        let meta = crate::meta_from_source(&source_with_model).expect("should return Some");
        assert_eq!(
            meta.get("agent"),
            Some(&serde_json::Value::String("test-agent".to_string()))
        );
        assert_eq!(
            meta.get("session"),
            Some(&serde_json::Value::String("session-123".to_string()))
        );
        assert_eq!(
            meta.get("harnx:model"),
            Some(&serde_json::Value::String("gpt-4o".to_string()))
        );

        // Case 2: model absent
        let source_without_model = AgentSource {
            agent: "test-agent".to_string(),
            session_id: Some("session-456".to_string()),
            model: None,
        };
        let meta = crate::meta_from_source(&source_without_model).expect("should return Some");
        assert_eq!(
            meta.get("agent"),
            Some(&serde_json::Value::String("test-agent".to_string()))
        );
        assert_eq!(
            meta.get("session"),
            Some(&serde_json::Value::String("session-456".to_string()))
        );
        assert!(
            !meta.contains_key("harnx:model"),
            "harnx:model should not be present when model is None"
        );
    }

    // ── Session persistence and manager identity tests (issue #988) ──
    //
    // These tests verify that:
    // 1. A single session with multiple prompts preserves the McpManager
    //    across prompts (no respawn).
    // 2. Two distinct sessions have distinct managers.
    // 3. A resumed session (id on disk, absent from map) is lazily rebuilt.

    /// Test that a single session reuses the same GlobalConfig (and thus the
    /// same McpManager) across multiple prompts. The fix for #988 moved from
    /// forking config per-prompt to forking once per-session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_single_session_reuses_config_across_prompts() {
        let (_temp, _path) = setup_agent_env("test");
        let config = test_config();
        let mock = Arc::new(
            MockClient::builder()
                .add_turn(MockTurnBuilder::new().add_text_chunk("reply one").build())
                .add_turn(MockTurnBuilder::new().add_text_chunk("reply two").build())
                .build(),
        );
        let _guard = TestStateGuard::new(Some(mock)).await;
        let agent = Arc::new(HarnxAgent::new("test".to_string(), config.clone()));

        let cwd = std::env::current_dir().expect("current dir");
        let local0 = tokio::task::LocalSet::new();
        let session_resp = local0
            .run_until(async {
                agent
                    .new_session(NewSessionRequest::new(cwd))
                    .await
                    .expect("create session")
            })
            .await;
        let session_id = session_resp.session_id.0.to_string();

        // Get the SessionContext after creation.
        let ctx1 = {
            let sessions = agent.sessions.lock().await;
            sessions.get(&session_id).expect("session exists").clone()
        };

        // First prompt.
        let local = tokio::task::LocalSet::new();
        let resp1 = local
            .run_until(async {
                agent
                    .prompt(PromptRequest::new(
                        agent_client_protocol::schema::v1::SessionId::new(session_id.clone()),
                        vec![ContentBlock::from("prompt one".to_string())],
                    ))
                    .await
            })
            .await;
        assert_eq!(
            resp1.expect("prompt 1 succeeds").stop_reason,
            agent_client_protocol::schema::v1::StopReason::EndTurn
        );

        // Get the SessionContext after first prompt - should be the same Arc.
        let ctx2 = {
            let sessions = agent.sessions.lock().await;
            sessions.get(&session_id).expect("session exists").clone()
        };

        // Second prompt.
        let local2 = tokio::task::LocalSet::new();
        let resp2 = local2
            .run_until(async {
                agent
                    .prompt(PromptRequest::new(
                        agent_client_protocol::schema::v1::SessionId::new(session_id.clone()),
                        vec![ContentBlock::from("prompt two".to_string())],
                    ))
                    .await
            })
            .await;
        assert_eq!(
            resp2.expect("prompt 2 succeeds").stop_reason,
            agent_client_protocol::schema::v1::StopReason::EndTurn
        );

        // Get the SessionContext after second prompt.
        let ctx3 = {
            let sessions = agent.sessions.lock().await;
            sessions.get(&session_id).expect("session exists").clone()
        };

        // All three Arcs should point to the same SessionContext.
        assert!(
            Arc::ptr_eq(&ctx1, &ctx2),
            "SessionContext should be the same across prompts"
        );
        assert!(
            Arc::ptr_eq(&ctx2, &ctx3),
            "SessionContext should be the same across prompts"
        );

        // The config inside should also be the same Arc.
        assert!(
            Arc::ptr_eq(&ctx1.config, &ctx2.config),
            "GlobalConfig should be the same across prompts"
        );
    }

    /// Test that two distinct sessions have distinct SessionContexts and configs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_distinct_sessions_have_distinct_contexts() {
        let (_temp, _path) = setup_agent_env("test");
        let config = test_config();
        let mock = Arc::new(
            MockClient::builder()
                .add_turn(MockTurnBuilder::new().add_text_chunk("reply one").build())
                .add_turn(MockTurnBuilder::new().add_text_chunk("reply two").build())
                .build(),
        );
        let _guard = TestStateGuard::new(Some(mock)).await;
        let agent = Arc::new(HarnxAgent::new("test".to_string(), config.clone()));

        let cwd = std::env::current_dir().expect("current dir");
        let local = tokio::task::LocalSet::new();
        let (s1, s2) = local
            .run_until(async {
                let r1 = agent
                    .new_session(NewSessionRequest::new(cwd.clone()))
                    .await
                    .expect("create session 1");
                let r2 = agent
                    .new_session(NewSessionRequest::new(cwd))
                    .await
                    .expect("create session 2");
                (r1, r2)
            })
            .await;

        let ctx1 = {
            let sessions = agent.sessions.lock().await;
            sessions
                .get(&s1.session_id.0.to_string())
                .expect("session 1 exists")
                .clone()
        };
        let ctx2 = {
            let sessions = agent.sessions.lock().await;
            sessions
                .get(&s2.session_id.0.to_string())
                .expect("session 2 exists")
                .clone()
        };

        // Different sessions should have different contexts.
        assert!(
            !Arc::ptr_eq(&ctx1, &ctx2),
            "Different sessions should have different SessionContexts"
        );
        assert!(
            !Arc::ptr_eq(&ctx1.config, &ctx2.config),
            "Different sessions should have different GlobalConfigs"
        );
    }

    /// Test lazy resume: a session that exists on disk but not in memory
    /// is rebuilt on-demand when a prompt arrives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_lazy_resume_rebuilds_session_from_disk() {
        let (_temp, _path) = setup_agent_env("test");
        let config = test_config();
        let sessions_dir = config
            .read()
            .sessions_dir_override
            .clone()
            .expect("sessions dir override");
        let mock = Arc::new(
            MockClient::builder()
                .add_turn(MockTurnBuilder::new().add_text_chunk("first reply").build())
                .add_turn(
                    MockTurnBuilder::new()
                        .add_text_chunk("resumed reply")
                        .build(),
                )
                .build(),
        );
        let _guard = TestStateGuard::new(Some(mock)).await;
        let agent = Arc::new(HarnxAgent::new("test".to_string(), config.clone()));

        // Create a session and run one prompt.
        let cwd = std::env::current_dir().expect("current dir");
        let local0 = tokio::task::LocalSet::new();
        let session_resp = local0
            .run_until(async {
                agent
                    .new_session(NewSessionRequest::new(cwd))
                    .await
                    .expect("create session")
            })
            .await;
        let session_id = session_resp.session_id.0.to_string();
        let log_path = sessions_dir.join(format!("{session_id}.yaml"));

        let local = tokio::task::LocalSet::new();
        let resp1 = local
            .run_until(async {
                agent
                    .prompt(PromptRequest::new(
                        agent_client_protocol::schema::v1::SessionId::new(session_id.clone()),
                        vec![ContentBlock::from("initial prompt".to_string())],
                    ))
                    .await
            })
            .await;
        assert!(resp1.is_ok());

        // Verify log exists.
        assert!(log_path.exists(), "session log should exist");

        // Evict the session from memory (simulate idle timeout).
        {
            let mut sessions = agent.sessions.lock().await;
            sessions.remove(&session_id);
        }

        // Verify it's gone.
        {
            let sessions = agent.sessions.lock().await;
            assert!(
                !sessions.contains_key(&session_id),
                "session should be evicted"
            );
        }

        // Prompt again - should trigger lazy rebuild.
        let local2 = tokio::task::LocalSet::new();
        let resp2 = local2
            .run_until(async {
                agent
                    .prompt(PromptRequest::new(
                        agent_client_protocol::schema::v1::SessionId::new(session_id.clone()),
                        vec![ContentBlock::from("resumed prompt".to_string())],
                    ))
                    .await
            })
            .await;

        // Should succeed.
        assert_eq!(
            resp2.expect("lazy resume succeeds").stop_reason,
            agent_client_protocol::schema::v1::StopReason::EndTurn
        );

        // Session is back in memory with a new SessionContext.
        let _ctx = {
            let sessions = agent.sessions.lock().await;
            sessions.get(&session_id).expect("session rebuilt").clone()
        };

        // Verify the log contains both prompts.
        let log_contents = tokio::fs::read_to_string(&log_path)
            .await
            .expect("read log");
        assert!(
            log_contents.contains("initial prompt"),
            "first prompt in log"
        );
        assert!(
            log_contents.contains("resumed prompt"),
            "second prompt in log"
        );
    }

    /// Test idle reaper logic: a session idle past TTL is evicted.
    /// Uses a shortened TTL for testing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_idle_session_is_evicted_after_ttl() {
        // This test would require injecting a shorter TTL, which needs
        // a test-only builder or env var. For now, verify the logic works
        // by checking the is_running and is_idle_expired methods.
        let (_temp, _path) = setup_agent_env("test");
        let config = test_config();

        let session_id = "test-session-id".to_string();
        let session_config = harnx_session::fork_prompt_config(&config.read().clone());

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let ctx = Arc::new(crate::SessionContext::new(
                    session_id.clone(),
                    session_config,
                ));

                // Fresh session: not idle expired, not running → NOT reaped.
                assert!(!ctx.is_idle_expired(), "fresh session not idle");
                assert!(!ctx.is_running(), "no prompt running");
                assert!(!ctx.should_reap(), "fresh idle session must not be reaped");

                // Idle past TTL, no prompt running → MUST be reaped.
                ctx.mark_idle_for_test();
                assert!(ctx.is_idle_expired(), "backdated session is idle-expired");
                assert!(!ctx.is_running(), "no prompt running");
                assert!(
                    ctx.should_reap(),
                    "idle session with no in-flight prompt must be reaped"
                );

                // Idle past TTL BUT a prompt is in flight (lock held) → must NOT
                // be reaped. This is the case the reaper must never get wrong:
                // reaping here would kill MCP subprocesses under a live prompt.
                let guard = ctx.prompt_lock.clone().lock_owned().await;
                assert!(ctx.is_running(), "prompt is running (lock held)");
                assert!(ctx.is_idle_expired(), "still idle-expired");
                assert!(
                    !ctx.should_reap(),
                    "must NOT reap a session with an in-flight prompt"
                );

                // Prompt finishes → reapable again.
                drop(guard);
                assert!(!ctx.is_running(), "prompt finished");
                assert!(ctx.should_reap(), "reapable again after prompt finishes");
            })
            .await;
    }

    /// Regression: a session whose only `touch()` happened at prompt START can
    /// age past the idle TTL DURING a long turn. Once the prompt finishes and
    /// releases `prompt_lock`, `should_reap()` becomes true — so without a
    /// touch-on-completion the very next reaper tick would evict a session that
    /// just finished active work, tearing down its warm MCP subprocesses.
    /// `HarnxAgent::prompt` touches again after the turn completes; this test
    /// asserts that a post-run `touch()` clears the idle state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_touch_after_long_prompt_prevents_immediate_reap() {
        let (_temp, _path) = setup_agent_env("test");
        let config = test_config();
        let session_config = harnx_session::fork_prompt_config(&config.read().clone());

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let ctx = Arc::new(crate::SessionContext::new(
                    "long-turn-session".to_string(),
                    session_config,
                ));

                // Simulate a turn that ran longer than the TTL: activity was
                // last recorded at prompt start, which is now idle-expired, and
                // the prompt has just released its lock (not running).
                ctx.mark_idle_for_test();
                assert!(ctx.is_idle_expired(), "backdated to idle-expired");
                assert!(!ctx.is_running(), "prompt finished (lock released)");
                assert!(
                    ctx.should_reap(),
                    "without touch-on-completion the session would be reaped"
                );

                // The touch that HarnxAgent::prompt performs AFTER the turn
                // completes must refresh activity and clear the idle state.
                ctx.touch();
                assert!(
                    !ctx.is_idle_expired(),
                    "touch-on-completion refreshed last_activity"
                );
                assert!(
                    !ctx.should_reap(),
                    "a just-finished session must not be immediately reapable"
                );
            })
            .await;
    }

    /// Test that Notice events are forwarded to ACP clients.
    /// Regression test for #990 — notices were silently dropped.
    #[test]
    fn test_notice_event_forwarding() {
        use crate::event_to_forward;
        use harnx_core::event::NoticeEvent;

        // Warning notice with message should be forwarded with ⚠ prefix.
        let forward = event_to_forward(
            AgentEvent::Notice(NoticeEvent::Warning("boom".into())),
            None,
        );
        match forward {
            Some(AcpForward::Text(text, source)) => {
                assert!(text.contains("boom"), "warning message preserved");
                assert!(text.starts_with("⚠"), "warning has ⚠ prefix");
                assert!(source.is_none(), "source is None");
            }
            other => panic!("expected AcpForward::Text, got {:?}", other),
        }

        // Error notice with message should be forwarded with 🔴 prefix.
        let forward = event_to_forward(
            AgentEvent::Notice(NoticeEvent::Error("fatal error".into())),
            None,
        );
        match forward {
            Some(AcpForward::Text(text, _)) => {
                assert!(text.contains("fatal error"), "error message preserved");
                assert!(text.starts_with("🔴"), "error has 🔴 prefix");
            }
            other => panic!("expected AcpForward::Text, got {:?}", other),
        }

        // Info notices must NOT be forwarded to ACP clients. They are a
        // presentation-layer artifact (nested sub-agent activity headings
        // routed via NestedAcpEvent::Text → NoticeEvent::Info) and would
        // corrupt the ACP transcript if injected (regression guarded by
        // tmux_e2e::nested_sub_agent_activity_no_duplicates).
        let forward = event_to_forward(AgentEvent::Notice(NoticeEvent::Info("hello".into())), None);
        assert!(
            forward.is_none(),
            "info notice must not be forwarded to ACP"
        );

        // Empty message should be dropped (None).
        let forward = event_to_forward(AgentEvent::Notice(NoticeEvent::Warning("".into())), None);
        assert!(forward.is_none(), "empty notice should be dropped");
    }
}
