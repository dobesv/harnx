//! Integration tests for session title generation and transcript construction.

use anyhow::Result;
use harnx_core::message::{Message, MessageContent, MessageRole};
use harnx_core::require_nextest;
use harnx_core::session::{Session, SessionLogEntry};
use harnx_runtime::config::session_ops_title::{
    build_title_transcript, DEFAULT_TITLE_SYSTEM_PROMPT,
};

/// A `Title` log entry round-trips through YAML and is NOT treated as Unknown.
#[test]
fn title_log_entry_round_trips() -> Result<()> {
    require_nextest();

    let yaml = "type: title\ntitle: Debugging async lifetimes\n";
    let entry: SessionLogEntry = serde_yaml::from_str(yaml)?;
    match &entry {
        SessionLogEntry::Title { title, .. } => assert_eq!(title, "Debugging async lifetimes"),
        other => panic!("expected Title, got {other:?}"),
    }
    let reserialized = serde_yaml::to_string(&entry)?;
    assert!(reserialized.contains("type: title"));
    assert!(reserialized.contains("title: Debugging async lifetimes"));
    Ok(())
}

/// The transcript builder captures the first user message and the last user
/// message (when different), and the assistant response after the last user
/// message — mirroring the TUI exit-summary heuristic.
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
    // Assistant reply preceding the last user message is excluded.
    assert!(!transcript.contains("first answer"));
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
