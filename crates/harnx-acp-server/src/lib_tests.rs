#[cfg(test)]
mod tests {
    use crate::{AcpChunkSink, AcpForward, HarnxAgent};
    use agent_client_protocol::schema::{
        CancelNotification, ContentBlock, NewSessionRequest, PromptRequest, PromptResponse,
    };
    use harnx_core::event::{AgentEvent, AgentEventSink, ToolEvent, ToolStatus};
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

    #[tokio::test]
    async fn test_new_session_returns_unique_ids() {
        let (_temp, _path) = setup_agent_env("test");
        let _guard = TestStateGuard::new(None).await;
        let config = test_config();
        let agent = HarnxAgent::new("test".to_string(), config);
        let cwd = std::env::current_dir().expect("current dir");

        let resp1 = agent
            .new_session(NewSessionRequest::new(cwd.clone()))
            .await
            .expect("create first session");
        let resp2 = agent
            .new_session(NewSessionRequest::new(cwd))
            .await
            .expect("create second session");
        let session_id1 = resp1.session_id.0.to_string();
        let session_id2 = resp2.session_id.0.to_string();

        assert_ne!(resp1.session_id, resp2.session_id);
        let sessions = agent.sessions.lock().await;
        assert!(sessions.contains_key(session_id1.as_str()));
        assert!(sessions.contains_key(session_id2.as_str()));
    }

    #[tokio::test]
    async fn test_cancel_marks_session() {
        let (_temp, _path) = setup_agent_env("test");
        let _guard = TestStateGuard::new(None).await;
        let config = test_config();
        let agent = HarnxAgent::new("test".to_string(), config);
        let cwd = std::env::current_dir().expect("current dir");

        let response = agent
            .new_session(NewSessionRequest::new(cwd))
            .await
            .expect("create session");
        let session_id = response.session_id.0.to_string();

        agent
            .cancel(CancelNotification::new(session_id.clone()))
            .await
            .expect("cancel session");

        let sessions = agent.sessions.lock().await;
        let session = sessions.get(session_id.as_str()).expect("stored session");
        assert!(session.abort_signal.aborted());
    }

    #[tokio::test]
    async fn test_cancel_unknown_session_errors() {
        let config = test_config();
        let agent = HarnxAgent::new("test".to_string(), config);

        let result = agent
            .cancel(CancelNotification::new("nonexistent".to_string()))
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
            let id = agent
                .new_session(NewSessionRequest::new(cwd))
                .await
                .expect("create session")
                .session_id
                .0
                .to_string();
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
                agent_client_protocol::schema::SessionId::new(self.id.clone()),
                vec![ContentBlock::from(text.to_string())],
            );
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
                agent_client_protocol::schema::StopReason::EndTurn
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
        use agent_client_protocol::schema::{InitializeRequest, ProtocolVersion};

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
}
