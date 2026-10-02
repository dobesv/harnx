use crate::config::*;
use harnx_core::message::MessageRole;

// ── handoff session emptying tests ─────────────────────────────────────

/// Verify that empty_session clears messages from a session that was loaded
/// with an existing name (simulating the handoff path with session_id).
/// This is the unit-level guarantee behind the #291 fix: after handoff the
/// new agent starts with a blank session even when a session_id was provided.
#[test]
fn test_new_session_has_session_id() {
    let config = Config::default();
    let session = self::session::new(&config, "metadata-check", None).unwrap();

    assert!(session.session_id.is_some());
}

#[test]
fn empty_session_after_persisted_clear_clears_named_session_with_messages() {
    let mut config = Config::default();
    let mut session = self::session::new(&config, "handoff-target", None).unwrap();
    session.push_message_for_test(MessageRole::System, "You are agent A.".to_string());
    session.push_message_for_test(MessageRole::User, "Hello from old session".to_string());
    session.push_message_for_test(MessageRole::Assistant, "Response from agent A".to_string());
    assert!(!session.is_empty());
    config.session = Some(session);

    config.empty_session_after_persisted_clear().unwrap();

    let session = config.session.as_ref().unwrap();
    assert!(
        session.is_empty(),
        "session should be empty after empty_session"
    );
}

#[test]
fn empty_session_keeps_messages_when_clear_cannot_be_persisted() {
    let mut config = Config::default();
    let mut session = self::session::new(&config, "handoff-target", None).unwrap();
    session.push_message_for_test(MessageRole::User, "keep me".to_string());
    config.session = Some(session);

    assert!(config.empty_session().is_err());
    assert!(!config.session.as_ref().unwrap().is_empty());
}

// ── after_chat_completion incremental persistence tests ─────────────────

/// Verify that after_chat_completion persists intermediate rounds
/// (non-empty tool_results) to the session, not just the final round.
#[tokio::test]
async fn after_chat_completion_saves_intermediate_tool_rounds() {
    use crate::tool::{ToolCall, ToolResult};
    use serde_json::json;

    let _tmp = tempfile::TempDir::new().unwrap();
    let mut config = Config {
        data: ConfigData {
            stream: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut session = self::session::new(&config, "test-intermediate", None).unwrap();
    self::session::attach_memory_log(&mut session);
    config.session = Some(session);

    let _agent = config.extract_agent();
    let global_config: GlobalConfig = Arc::new(crate::config::ConfigLock::new(config));
    let input = crate::config::input::from_str(&global_config, "do something", None);

    let tool_results = vec![ToolResult::new(
        ToolCall::new(
            "my_tool".to_string(),
            json!({"key": "val"}),
            Some("tc1".to_string()),
            None,
        ),
        json!("tool output"),
    )];

    // Call after_chat_completion with non-empty tool_results.
    // Previously this returned early without saving; now it should persist.
    let request = SessionSaveRequest::new(&input, "intermediate output", None);
    let persistence = {
        global_config
            .write()
            .prepare_after_chat_completion(&request, &tool_results, &Default::default())
            .unwrap()
    };
    persistence.persist().await;

    let config_guard = global_config.read();
    let session = config_guard.session.as_ref().unwrap();
    assert!(
        !session.is_empty(),
        "session should have messages after intermediate round"
    );
    // Verify content via the session's export (which serializes messages).
    let export = session.export().unwrap();
    assert!(
        export.contains("intermediate output"),
        "session export should contain assistant output; got:\n{export}"
    );
    assert!(
        export.contains("my_tool"),
        "session export should contain tool call info; got:\n{export}"
    );
}
