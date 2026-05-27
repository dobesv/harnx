#[cfg(test)]
mod tests {
    use crate::{AcpChunkSink, AcpForward, HarnxAgent};
    use agent_client_protocol::schema::{CancelNotification, NewSessionRequest};
    use harnx_core::event::{AgentEvent, AgentEventSink, ToolEvent, ToolStatus};
    use harnx_runtime::{
        client::{ClientConfig, ModelType, TestStateGuard},
        config::Config,
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
        assert_eq!(meta.get("agent"), Some(&serde_json::Value::String("test-agent".to_string())));
        assert_eq!(meta.get("session"), Some(&serde_json::Value::String("session-123".to_string())));
        assert_eq!(meta.get("harnx:model"), Some(&serde_json::Value::String("gpt-4o".to_string())));

        // Case 2: model absent
        let source_without_model = AgentSource {
            agent: "test-agent".to_string(),
            session_id: Some("session-456".to_string()),
            model: None,
        };
        let meta = crate::meta_from_source(&source_without_model).expect("should return Some");
        assert_eq!(meta.get("agent"), Some(&serde_json::Value::String("test-agent".to_string())));
        assert_eq!(meta.get("session"), Some(&serde_json::Value::String("session-456".to_string())));
        assert!(!meta.contains_key("harnx:model"), "harnx:model should not be present when model is None");
    }
}
