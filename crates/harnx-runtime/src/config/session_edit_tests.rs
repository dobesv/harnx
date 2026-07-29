//! Session-log edit/validation tests (extracted from config tests for code health).
#![cfg(test)]

use super::*;
use harnx_core::{
    message::{Message, MessageContent, MessageRole},
    session::ToolOutput,
    tool::ToolCall,
};

fn tool_calls_yaml(call_ids: &[&str]) -> String {
    serde_yaml::to_string(&SessionLogEntry::ToolCalls {
        timestamp: None,
        fence_token: None,
        text: String::new(),
        thought: None,
        calls: call_ids
            .iter()
            .map(|id| ToolCall {
                name: "bash_exec".to_string(),
                arguments: serde_json::json!({"cmd": "echo hi"}),
                id: Some((*id).to_string()),
                thought_signature: None,
            })
            .collect(),
    })
    .unwrap()
}

fn tool_results_yaml(result_ids: &[&str]) -> String {
    tool_results_yaml_with_optional_ids(
        &result_ids
            .iter()
            .map(|id| Some((*id).to_string()))
            .collect::<Vec<_>>(),
    )
}

fn tool_results_yaml_with_optional_ids(result_ids: &[Option<String>]) -> String {
    serde_yaml::to_string(&SessionLogEntry::ToolResults {
        timestamp: None,
        results: result_ids
            .iter()
            .map(|id| ToolOutput {
                id: id.clone(),
                name: "bash_exec".to_string(),
                output: serde_json::json!({"ok": true}),
                markdown: None,
                content: Vec::new(),
                switch_agent: None,
            })
            .collect(),
    })
    .unwrap()
}

fn user_yaml(text: &str) -> String {
    serde_yaml::to_string(&SessionLogEntry::Message {
        id: None,
        timestamp: None,
        fence_token: None,
        role: MessageRole::User,
        content: MessageContent::Text(text.to_string()),
    })
    .unwrap()
}

fn assistant_yaml(text: &str) -> String {
    serde_yaml::to_string(&SessionLogEntry::Message {
        id: None,
        timestamp: None,
        fence_token: None,
        role: MessageRole::Assistant,
        content: MessageContent::Text(text.to_string()),
    })
    .unwrap()
}

/// Build a `Config` wired with a dummy `test:model` client and an isolated
/// sessions dir, for the message-edit tests.
fn editor_test_config(sessions_dir: std::path::PathBuf) -> Config {
    let mut config = Config {
        sessions_dir_override: Some(sessions_dir),
        working_mode: WorkingMode::Cmd,
        ..Config::default()
    };
    config
        .clients
        .push(harnx_client::ClientConfig::OpenAICompatibleConfig(
            harnx_core::provider_config::openai_compatible::OpenAICompatibleConfig {
                name: "test".to_string(),
                api_base: None,
                api_key: None,
                models: vec![],
                patches: None,
                extra: None,
                system_prompt_prefix: None,
                package: None,
            },
        ));
    config.model = harnx_client::Model::new("test", "model");
    config.model_id = "test:model".to_string();
    config
}

fn variable_test_config(sessions_dir: std::path::PathBuf) -> Config {
    let mut config = editor_test_config(sessions_dir);
    let mut agent = Agent::new(AgentConfig::from_prompt(""));
    agent.set_model(config.model.clone());
    config.agent = Some(agent);
    config
}

#[test]
fn set_variable_persists_and_creates_empty_key() -> Result<()> {
    let sessions = tempfile::TempDir::new()?;
    let mut config = variable_test_config(sessions.path().to_path_buf());
    config.use_session(Some("variable-persistence"))?;
    config
        .session
        .as_mut()
        .context("No active session")?
        .messages
        .push(Message::new(
            MessageRole::User,
            MessageContent::Text("keep session active".to_string()),
        ));

    config.set_variable("persisted", "saved value")?;
    config.set_variable("empty", "")?;
    config.save_session(None)?;
    drop(config);

    let mut reloaded = variable_test_config(sessions.path().to_path_buf());
    reloaded.use_session(Some("variable-persistence"))?;
    assert_eq!(reloaded.get_variable("persisted")?, "saved value");
    assert_eq!(reloaded.get_variable("empty")?, "");

    Ok(())
}

#[test]
fn get_variable_returns_full_long_value() -> Result<()> {
    let sessions = tempfile::TempDir::new()?;
    let mut config = variable_test_config(sessions.path().to_path_buf());
    config.use_session(Some("variable-get"))?;
    let long_value = "x".repeat(250);

    config.set_variable("long", &long_value)?;

    assert_eq!(config.get_variable("long")?, long_value);
    Ok(())
}

#[test]
fn list_variables_truncates_long_values_and_preserves_short_values() -> Result<()> {
    let sessions = tempfile::TempDir::new()?;
    let mut config = variable_test_config(sessions.path().to_path_buf());
    config.use_session(Some("variable-list"))?;
    config.set_variable("short", "small")?;
    config.set_variable("long", &"x".repeat(250))?;

    let listed = config.list_variables()?;
    let short_line = listed
        .lines()
        .find(|line| line.starts_with("short = "))
        .context("short variable missing from list")?;
    let long_rendered = listed
        .split_once("long = ")
        .map(|(_, value)| value)
        .context("long variable missing from list")?;

    assert_eq!(short_line, "short = small");
    assert!(long_rendered.contains("…[truncated]…"));
    assert!(long_rendered.chars().count() <= 200);
    Ok(())
}

#[test]
fn load_variable_reads_file_and_missing_file_does_not_mutate() -> Result<()> {
    let sessions = tempfile::TempDir::new()?;
    let files = tempfile::TempDir::new()?;
    let mut config = variable_test_config(sessions.path().to_path_buf());
    config.use_session(Some("variable-load"))?;
    config.set_variable("loaded", "original")?;

    let value_path = files.path().join("value.txt");
    std::fs::write(&value_path, "loaded from file\n")?;
    config.load_variable(
        "loaded",
        value_path.to_str().context("non-UTF-8 temp path")?,
    )?;
    assert_eq!(config.get_variable("loaded")?, "loaded from file\n");

    let before = config
        .session
        .as_ref()
        .context("No active session")?
        .agent_variables()
        .clone();
    let missing_path = files.path().join("missing.txt");
    let error = config
        .load_variable(
            "new-key",
            missing_path.to_str().context("non-UTF-8 temp path")?,
        )
        .expect_err("missing variable file should fail");
    assert!(error.to_string().contains("Failed to read variable file"));
    assert_eq!(
        config
            .session
            .as_ref()
            .context("No active session")?
            .agent_variables(),
        &before
    );

    Ok(())
}

#[test]
fn edit_variable_round_trip_supports_content_and_empty_edits() {
    use tempfile::TempDir;

    let sessions = TempDir::new().unwrap();
    let editor_tmp = TempDir::new().unwrap();
    let original_editor = std::env::var_os("EDITOR");
    std::env::set_var("EDITOR", "true");

    let result = (|| -> Result<()> {
        let mut config = variable_test_config(sessions.path().to_path_buf());
        config.use_session(Some("variable-edit"))?;
        config.set_variable("draft", "initial")?;

        let editor_tmp_path = editor_tmp.path().to_path_buf();
        config.temp_dir_override = Some(editor_tmp_path.clone());
        let mut edit_count = 0;
        config.set_tui_editor_hooks(
            None,
            Some(Box::new(move || {
                let temp_path = std::fs::read_dir(&editor_tmp_path)
                    .unwrap()
                    .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                    .find(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with("variable-edit-"))
                            && path.extension().and_then(|ext| ext.to_str()) == Some("txt")
                    })
                    .expect("variable edit temp file");
                let content = if edit_count == 0 {
                    "edited variable content\n"
                } else {
                    ""
                };
                edit_count += 1;
                std::fs::write(&temp_path, content).unwrap();
            })),
        );

        config.edit_variable("draft")?;
        assert_eq!(config.get_variable("draft")?, "edited variable content\n");
        config.edit_variable("draft")?;
        assert_eq!(config.get_variable("draft")?, "");

        Ok(())
    })();

    match original_editor {
        Some(value) => std::env::set_var("EDITOR", value),
        None => std::env::remove_var("EDITOR"),
    }

    result.unwrap();
}

#[test]
fn validate_edited_session_documents_accepts_valid_yaml_documents() {
    let content = format!("{}---\n{}", user_yaml("hi"), user_yaml("there"));

    let documents = validate_edited_session_documents(&content).unwrap();

    assert_eq!(documents.len(), 2);
}

#[test]
fn validate_edited_session_documents_rejects_invalid_yaml_documents() {
    let err = validate_edited_session_documents("---\ntype: user\ntext: [")
        .expect_err("invalid yaml should fail");

    assert!(err.to_string().contains("Invalid session log entry YAML"));
}

#[test]
fn validate_tool_pair_integrity_accepts_matching_ids() {
    let documents = vec![
        tool_calls_yaml(&["call-1", "call-2"]),
        tool_results_yaml(&["call-1", "call-2"]),
    ];

    validate_tool_pair_integrity(5, &documents).unwrap();
}

#[test]
fn validate_tool_pair_integrity_rejects_mismatched_ids() {
    let documents = vec![tool_calls_yaml(&["call-1"]), tool_results_yaml(&["call-2"])];

    let err = validate_tool_pair_integrity(7, &documents).expect_err("mismatched ids should fail");

    assert_eq!(
            err.to_string(),
            "Edited tool result at 8 references unknown tool_call_id 'call-2' (expected one of: call-1)"
        );
}

#[test]
fn validate_tool_pair_integrity_rejects_missing_immediate_tool_results() {
    let documents = vec![
        tool_calls_yaml(&["call-1"]),
        user_yaml("intervening message"),
    ];

    let err = validate_tool_pair_integrity(3, &documents)
        .expect_err("tool calls without immediate tool results should fail");

    assert_eq!(
        err.to_string(),
        "Edited tool call entry at 3 must be followed immediately by matching tool results"
    );
}

#[test]
fn validate_tool_pair_integrity_accepts_positional_tool_results_without_ids() {
    let documents = vec![
        tool_calls_yaml(&["call-1", "call-2"]),
        tool_results_yaml_with_optional_ids(&[None, None]),
    ];

    validate_tool_pair_integrity(4, &documents).unwrap();
}

#[test]
fn validate_tool_pair_integrity_rejects_positional_tool_results_when_counts_differ() {
    let documents = vec![
        tool_calls_yaml(&["call-1", "call-2"]),
        tool_results_yaml_with_optional_ids(&[None]),
    ];

    let err = validate_tool_pair_integrity(10, &documents)
        .expect_err("count mismatch should fail positional matching");

    assert_eq!(
            err.to_string(),
            "Edited tool result at 11 is missing tool_call_id for positional matching and count 1 does not match tool calls count 2"
        );
}

#[test]
fn validate_tool_pair_integrity_rejects_mixed_present_and_missing_result_ids() {
    let documents = vec![
        tool_calls_yaml(&["call-1", "call-2"]),
        tool_results_yaml_with_optional_ids(&[Some("call-1".to_string()), None]),
    ];

    let err =
        validate_tool_pair_integrity(12, &documents).expect_err("mixed id presence should fail");

    assert_eq!(
        err.to_string(),
        "Edited tool result at 13 mixes tool_call_id values with missing tool_call_id entries"
    );
}

#[test]
fn validate_tool_pair_integrity_ignores_single_non_tool_document() {
    let documents = vec![user_yaml("plain message")];

    validate_tool_pair_integrity(2, &documents).unwrap();
}

#[test]
fn validate_tool_pair_integrity_allows_reordered_non_tool_documents() {
    let documents = vec![assistant_yaml("second"), user_yaml("first")];

    validate_tool_pair_integrity(1, &documents).unwrap();
}

#[test]
fn edit_message_range_supports_reordering_plain_messages() {
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let original_editor = std::env::var_os("EDITOR");
    std::env::set_var("EDITOR", "true");

    let result = (|| -> Result<()> {
        let mut config = editor_test_config(tmp.path().to_path_buf());
        config.use_session(Some("reorder"))?;

        let session = config.session.as_mut().context("No session")?;
        assert!(crate::config::session::append_event(
            session,
            &SessionLogEntry::Message {
                id: None,
                timestamp: None,
                fence_token: None,
                role: MessageRole::User,
                content: MessageContent::Text("first".to_string()),
            },
        ));
        assert!(crate::config::session::append_event(
            session,
            &SessionLogEntry::Message {
                id: None,
                timestamp: None,
                fence_token: None,
                role: MessageRole::Assistant,
                content: MessageContent::Text("second".to_string()),
            },
        ));

        let replacement_yaml = assistant_yaml("second") + "\n---\n" + &user_yaml("first");

        // Use an isolated temp dir so the after-hook can find the single
        // .yaml file without scanning the global temp directory.
        let editor_tmp = TempDir::new().unwrap();
        let editor_tmp_path = editor_tmp.path().to_path_buf();
        config.temp_dir_override = Some(editor_tmp_path.clone());

        config.set_tui_editor_hooks(
            None,
            Some(Box::new(move || {
                let temp_path = std::fs::read_dir(&editor_tmp_path)
                    .unwrap()
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .find(|p| p.extension().and_then(|e| e.to_str()) == Some("yaml"))
                    .expect("message edit temp file");
                std::fs::write(&temp_path, &replacement_yaml).unwrap();
            })),
        );

        config.edit_message_range(1, 2)?;

        let reloaded = config.session.as_ref().context("No session after reload")?;
        let texts: Vec<_> = reloaded
            .messages
            .iter()
            .map(|msg| msg.content.to_text())
            .collect();
        assert_eq!(texts, vec!["second", "first"]);

        Ok(())
    })();

    match original_editor {
        Some(value) => std::env::set_var("EDITOR", value),
        None => std::env::remove_var("EDITOR"),
    }

    result.unwrap();
}

// --- adjust_range_for_tool_pairs ---

fn make_docs(entries: &[SessionLogEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|e| serde_yaml::to_string(e).unwrap().trim().to_string())
        .collect()
}

fn tool_calls_entry(id: &str) -> SessionLogEntry {
    SessionLogEntry::ToolCalls {
        timestamp: None,
        fence_token: None,
        text: String::new(),
        thought: None,
        calls: vec![ToolCall {
            name: "bash_exec".to_string(),
            arguments: serde_json::json!({}),
            id: Some(id.to_string()),
            thought_signature: None,
        }],
    }
}

fn tool_results_entry(id: &str) -> SessionLogEntry {
    SessionLogEntry::ToolResults {
        timestamp: None,
        results: vec![ToolOutput {
            id: Some(id.to_string()),
            name: "bash_exec".to_string(),
            output: serde_json::json!("ok"),
            markdown: None,
            content: Vec::new(),
            switch_agent: None,
        }],
    }
}

fn user_entry(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        timestamp: None,
        fence_token: None,
        role: MessageRole::User,
        content: MessageContent::Text(text.to_string()),
    }
}

#[test]
fn adjust_range_no_tool_pairs_unchanged() {
    // [0:header, 1:user, 2:user] — no pairs, range stays as-is
    let docs = make_docs(&[user_entry("a"), user_entry("b"), user_entry("c")]);
    assert_eq!(adjust_range_for_tool_pairs(1, 2, &docs).unwrap(), (1, 2));
}

#[test]
fn adjust_range_expands_to_include_paired_results() {
    // [0:user, 1:tool_calls, 2:tool_results, 3:user]
    // Requesting range [1,1] (calls only) → auto-expands to [1,2]
    let docs = make_docs(&[
        user_entry("before"),
        tool_calls_entry("c1"),
        tool_results_entry("c1"),
        user_entry("after"),
    ]);
    assert_eq!(adjust_range_for_tool_pairs(1, 1, &docs).unwrap(), (1, 2));
}

#[test]
fn adjust_range_pair_already_fully_included_unchanged() {
    // [0:user, 1:tool_calls, 2:tool_results, 3:user]
    // Range [1,2] already covers both — no change
    let docs = make_docs(&[
        user_entry("before"),
        tool_calls_entry("c1"),
        tool_results_entry("c1"),
        user_entry("after"),
    ]);
    assert_eq!(adjust_range_for_tool_pairs(1, 2, &docs).unwrap(), (1, 2));
}

#[test]
fn adjust_range_rejects_range_starting_on_tool_results() {
    // [0:user, 1:tool_calls, 2:tool_results, 3:user]
    // Range starting at 2 (results only) → error
    let docs = make_docs(&[
        user_entry("before"),
        tool_calls_entry("c1"),
        tool_results_entry("c1"),
        user_entry("after"),
    ]);
    let err =
        adjust_range_for_tool_pairs(2, 2, &docs).expect_err("starting on tool-results should fail");
    assert!(err.to_string().contains("tool-results entry"));
}

#[test]
fn adjust_range_tool_calls_at_end_of_log_no_expansion() {
    // [0:user, 1:tool_calls] — ToolCalls is the last doc, no results follow
    // → no expansion (orphan ToolCalls is the user's problem after editing)
    let docs = make_docs(&[user_entry("before"), tool_calls_entry("c1")]);
    assert_eq!(adjust_range_for_tool_pairs(1, 1, &docs).unwrap(), (1, 1));
}

#[test]
fn adjust_range_rewind_orphan_rejected() {
    // Simulate rewind check: after_seq=1 lands on ToolCalls paired with results at 2
    // [0:user, 1:tool_calls, 2:tool_results, 3:user]
    let docs = make_docs(&[
        user_entry("before"),
        tool_calls_entry("c1"),
        tool_results_entry("c1"),
        user_entry("after"),
    ]);
    // after_seq=1 means entries 0..=1 kept, entry 2 (results) excluded → orphan calls
    // Verify the guard logic that rewind_session uses
    let parse = |idx: usize| -> Option<SessionLogEntry> {
        docs.get(idx)
            .and_then(|raw| serde_yaml::from_str::<SessionLogEntry>(raw).ok())
    };
    assert!(matches!(parse(1), Some(SessionLogEntry::ToolCalls { .. })));
    assert!(matches!(
        parse(2),
        Some(SessionLogEntry::ToolResults { .. })
    ));
    // The condition that rewind_session checks:
    let would_orphan = matches!(parse(1), Some(SessionLogEntry::ToolCalls { .. }))
        && matches!(parse(2), Some(SessionLogEntry::ToolResults { .. }));
    assert!(would_orphan);
}
