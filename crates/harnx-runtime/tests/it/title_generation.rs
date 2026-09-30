//! Integration tests for session title generation and transcript construction.

use anyhow::Result;
use harnx_core::message::{Message, MessageContent, MessageRole};
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
