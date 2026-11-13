//! Integration tests for session title generation and transcript construction.

use anyhow::Result;
use harnx_core::message::{Message, MessageContent, MessageContentToolCalls, MessageRole};
use harnx_core::require_nextest;
use harnx_core::session::{Session, SessionLogEntry};
use harnx_runtime::config::session_ops_title::{
    build_title_transcript, DEFAULT_TITLE_SYSTEM_PROMPT,
};
use harnx_runtime::nats_session_metadata::{SessionInitializer, SessionMetadata, SessionTitle};

/// Title state round-trips in canonical metadata, not the transcript.
#[test]
fn title_metadata_round_trips() -> Result<()> {
    require_nextest();

    let mut metadata = SessionMetadata::new(
        "title-test",
        SessionInitializer::named("metis", Default::default()),
    );
    metadata.title = SessionTitle {
        value: Some("Debugging async lifetimes".to_string()),
        manual: true,
        last_updated_tokens: 42,
    };
    let encoded = serde_json::to_vec(&metadata)?;
    let decoded: SessionMetadata = serde_json::from_slice(&encoded)?;
    assert_eq!(decoded.title, metadata.title);

    let legacy: SessionLogEntry =
        serde_yaml::from_str("type: title\ntitle: legacy transcript title\n")?;
    assert!(matches!(legacy, SessionLogEntry::Unknown));
    Ok(())
}

/// The transcript builder captures the first user message and the last user
/// message (when different), and the assistant response after the last user
/// message, while retaining earlier discoveries for context.
#[test]
fn transcript_builder_selects_expected_sections() {
    require_nextest();

    let session = Session {
        messages: vec![
            Message::new(
                MessageRole::User,
                MessageContent::Text("set up postgres pooling".to_string()),
            ),
            Message::new(
                MessageRole::Assistant,
                MessageContent::Text("first answer".to_string()),
            ),
            Message::new(
                MessageRole::User,
                MessageContent::Text("now tune the pool size".to_string()),
            ),
            Message::new(
                MessageRole::Assistant,
                MessageContent::Text("final answer".to_string()),
            ),
        ],
        ..Session::default()
    };

    let transcript = build_title_transcript(&session);
    assert!(transcript.contains("set up postgres pooling"));
    assert!(transcript.contains("now tune the pool size"));
    assert!(transcript.contains("final answer"));
    // A follow-up must not erase the context established earlier.
    assert!(transcript.contains("first answer"));
}

/// The default title prompt is natural-language guidance (not a slug prompt).
#[test]
fn default_title_prompt_is_natural_language() {
    require_nextest();
    assert!(DEFAULT_TITLE_SYSTEM_PROMPT.contains("title"));
    assert!(
        DEFAULT_TITLE_SYSTEM_PROMPT
            .to_lowercase()
            .contains("no quotes")
            || DEFAULT_TITLE_SYSTEM_PROMPT
                .to_lowercase()
                .contains("plain text")
    );
    // Should discourage slug/kebab output.
    assert!(DEFAULT_TITLE_SYSTEM_PROMPT
        .to_lowercase()
        .contains("natural language"));
}

fn bash_result(command: &str, output: serde_json::Value) -> harnx_core::tool::ToolResult {
    use harnx_core::tool::{ToolCall, ToolResult};
    ToolResult::new(
        ToolCall {
            name: "bash_exec".into(),
            arguments: serde_json::json!({"command": command}),
            ..ToolCall::default()
        },
        output,
    )
}

/// A URL-only request needs the issue downloaded by the tool, even after a
/// follow-up. Replayed tool rounds use the Tool role rather than Assistant.
#[test]
fn transcript_includes_fetched_issue_and_distinct_repeated_calls() {
    require_nextest();
    use serde_json::json;

    let results = vec![
        bash_result(
            "gh issue view 2695 --json title,body",
            json!({"content": [{"type": "text", "text": "Title generation is bad: include fetched issue contents", "annotations": {"audience": ["assistant"]}}]}),
        ),
        bash_result(
            "rg -n build_title_transcript crates",
            json!({"stdout": "session_ops_title.rs: build_title_transcript drops tool output", "exit_code": 0}),
        ),
    ];
    let session = Session {
        messages: vec![
            Message::new(
                MessageRole::System,
                MessageContent::Text("irrelevant system instructions".into()),
            ),
            Message::new(
                MessageRole::User,
                MessageContent::Text("https://github.com/dobesv/harnx/issues/2695".into()),
            ),
            Message::new(
                MessageRole::Tool,
                MessageContent::ToolCalls(MessageContentToolCalls::new(
                    results,
                    String::new(),
                    None,
                )),
            ),
            Message::new(
                MessageRole::User,
                MessageContent::Text("please fix it".into()),
            ),
            Message::new(
                MessageRole::Assistant,
                MessageContent::Text("Preserving tool context for session titles".into()),
            ),
        ],
        ..Session::default()
    };
    let transcript = build_title_transcript(&session);
    for expected in [
        "Title generation is bad",
        "include fetched issue contents",
        "gh issue view 2695",
        "rg -n build_title_transcript",
        "drops tool output",
        "please fix it",
        "Preserving tool context",
    ] {
        assert!(
            transcript.contains(expected),
            "missing {expected}: {transcript}"
        );
    }
    let first_call = "Tool: bash_exec\n\nArguments:\n{\"command\":\"gh issue view 2695 --json title,body\"}\n\nResult:\nTitle generation is bad: include fetched issue contents";
    let second_call = "Tool: bash_exec\n\nArguments:\n{\"command\":\"rg -n build_title_transcript crates\"}\n\nResult:\n{\"stdout\":\"session_ops_title.rs: build_title_transcript drops tool output\",\"exit_code\":0}";
    let first_index = transcript
        .find(first_call)
        .expect("first call must retain its arguments and result labels");
    let second_index = transcript
        .find(second_call)
        .expect("second call must retain its arguments and result labels");
    assert!(first_index < second_index);
    assert_eq!(transcript.matches("Tool: bash_exec").count(), 2);
    assert!(!transcript.contains("irrelevant system instructions"));
}

/// Large instructions, tool payloads, and long histories must not crowd out
/// both the initial discovery and the latest progress, including Unicode text.
#[test]
fn transcript_bounds_large_history_and_keeps_initial_and_latest_context() {
    require_nextest();
    use harnx_core::tool::{ToolCall, ToolResult};
    use serde_json::json;

    let mut messages = vec![
        Message::new(
            MessageRole::User,
            MessageContent::Text(format!("Investigate the issue {}", "界".repeat(10000))),
        ),
        Message::new(
            MessageRole::Tool,
            MessageContent::ToolCalls(MessageContentToolCalls::new(
                vec![ToolResult::new(
                    ToolCall {
                        name: "fetch".into(),
                        arguments: json!({"url": "https://example.com/issue"}),
                        ..ToolCall::default()
                    },
                    json!({"content": [{"type": "text", "text": format!("Issue: title model lacks tool context {}", "界".repeat(10000))}]}),
                )],
                String::new(),
                None,
            )),
        ),
    ];
    for i in 0..100 {
        messages.push(Message::new(
            MessageRole::Assistant,
            MessageContent::Text(format!("Intermediate progress {i} {}", "文".repeat(2000))),
        ));
    }
    messages.extend([
        Message::new(
            MessageRole::User,
            MessageContent::Text(format!(
                "Latest request: keep useful titles {}",
                "語".repeat(10000)
            )),
        ),
        Message::new(
            MessageRole::Assistant,
            MessageContent::Text("Latest result: regression coverage passes".into()),
        ),
    ]);
    let transcript = build_title_transcript(&Session {
        messages,
        ..Session::default()
    });
    assert!(transcript.chars().count() <= 8000);
    for expected in [
        "Investigate the issue",
        "Issue: title model lacks tool context",
        "Latest request: keep useful titles",
        "Latest result: regression coverage passes",
        "activity omitted",
    ] {
        assert!(
            transcript.contains(expected),
            "missing {expected}: {transcript}"
        );
    }
    assert!(!transcript.contains("Intermediate progress 50 "));
}
