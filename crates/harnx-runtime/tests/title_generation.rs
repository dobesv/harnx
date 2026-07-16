//! Integration tests for session title generation: log-entry serde, meta
//! extraction, backward compatibility with pre-title session files, and the
//! deterministic transcript builder.

use anyhow::Result;
use harnx_core::message::{Message, MessageContent, MessageRole};
use harnx_core::require_nextest;
use harnx_core::session::{Session, SessionLogEntry};
use harnx_runtime::config::parse_session_meta;
use harnx_runtime::config::session_ops_title::{
    build_title_transcript, DEFAULT_TITLE_SYSTEM_PROMPT,
};
use std::fs;
use tempfile::TempDir;

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

/// Write a session log file with the given body and return the title that
/// `parse_session_meta` extracts from it.
fn title_from_session_log(body: &str) -> Option<String> {
    let tmp = TempDir::new().expect("temp dir");
    let path = tmp.path().join("session.yaml");
    fs::write(&path, body).expect("write session file");
    parse_session_meta("session", &path)
        .expect("meta should parse")
        .title
}

/// A session file that contains a `Title` event surfaces it in `SessionMeta`.
#[test]
fn session_meta_reads_generated_title() {
    require_nextest();
    let title = title_from_session_log(concat!(
        "type: header\nmodel: test-model\nsession_id: sess-1\n",
        "---\ntype: message\nrole: user\ncontent: how do I fix this bug\n",
        "---\ntype: title\ntitle: Fixing the bug\n",
    ));
    assert_eq!(title.as_deref(), Some("Fixing the bug"));
}

/// An OLD session file with NO `Title` event must load cleanly with `title == None`.
#[test]
fn old_session_file_without_title_loads_with_none() {
    require_nextest();
    let title = title_from_session_log(concat!(
        "type: header\nmodel: test-model\nsession_id: sess-legacy\nagent_instructions: You are Oracle\n",
        "---\ntype: message\nrole: user\ncontent: hello\n",
        "---\ntype: message\nrole: assistant\ncontent: hi there\n",
    ));
    assert_eq!(title, None);
}

/// The LATEST title event is used when several are present, so regenerated and
/// manual titles are reflected in listings.
#[test]
fn session_meta_uses_latest_title_event() {
    require_nextest();
    let title = title_from_session_log(concat!(
        "type: header\nmodel: test-model\nsession_id: sess-multi\n",
        "---\ntype: title\ntitle: First Title\n",
        "---\ntype: title\ntitle: Second Title\n",
    ));
    assert_eq!(title.as_deref(), Some("Second Title"));
}

/// A manual title (`.set title`) written as a `Title { manual: true }` entry is
/// reflected in `SessionMeta` and replays with the freeze semantics.
#[test]
fn session_meta_reads_manual_title() {
    require_nextest();
    let title = title_from_session_log(concat!(
        "type: header\nmodel: test-model\nsession_id: sess-manual\n",
        "---\ntype: title\ntitle: My Custom Title\nmanual: true\n",
    ));
    assert_eq!(title.as_deref(), Some("My Custom Title"));
}

/// A title event past the first 64KB of a large session file is still found
/// (the scan covers the whole log, not a fixed prefix window).
#[test]
fn session_meta_finds_title_in_large_file() -> Result<()> {
    require_nextest();

    let tmp = TempDir::new()?;
    let path = tmp.path().join("huge.yaml");

    let mut content = String::from("type: header\nmodel: test-model\nsession_id: sess-huge\n");
    // Pad with many message documents so the title lands well past 128KB.
    let filler = "x".repeat(200);
    for i in 0..1500 {
        content.push_str("---\ntype: message\nrole: user\ncontent: ");
        content.push_str(&filler);
        content.push_str(&format!("{i}\n"));
    }
    // Title appears deep in the file, then more messages follow after it so the
    // title is neither in the first nor last 64KB window.
    content.push_str("---\ntype: title\ntitle: Deep Title\n");
    for i in 0..1500 {
        content.push_str("---\ntype: message\nrole: assistant\ncontent: ");
        content.push_str(&filler);
        content.push_str(&format!("{i}\n"));
    }
    fs::write(&path, &content)?;
    assert!(content.len() > 256 * 1024, "fixture must exceed 256KB");

    let meta = parse_session_meta("huge", &path).expect("meta should parse");
    assert_eq!(meta.title.as_deref(), Some("Deep Title"));
    Ok(())
}

/// The transcript builder captures the first user message, the last user
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
