//! compact_session / compaction-agent tests for the config module.
#![cfg(test)]

use super::test_support::EnvGuard;
use super::*;
use crate::client::TestStateGuard;
use crate::test_utils::{MockClient, MockTurnBuilder};
use harnx_core::message::{Message, MessageContent, MessageRole};
use std::io::Write as _;

// ── shared test helpers ──────────────────────────────────────────────────

/// Write `<dir>/<name>.md` (creating `dir`), for staging agent files a test
/// resolves via `HARNX_CONFIG_DIR`.
fn write_agent(dir: &std::path::Path, name: &str, content: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::File::create(dir.join(format!("{name}.md")))
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();
}

/// A stream-off `Config` whose current (non-session) agent is built from
/// `md`, with the gemini test model set.
fn config_with_agent(name: &str, md: &str) -> Config {
    let mut agent = Agent::new(AgentConfig::from_markdown(name, md).unwrap());
    agent.set_model(crate::client::Model::new("gemini", "gemini-3.1-flash-lite"));
    let mut config = Config {
        data: ConfigData {
            stream: false,
            ..Default::default()
        },
        ..Default::default()
    };
    config.agent = Some(agent);
    config
}

/// Attach a fresh session carrying `turns` and wrap the config for sharing.
fn with_session(mut config: Config, turns: Vec<(MessageRole, String)>) -> GlobalConfig {
    let mut session = self::session::new(&config, "test-session", None).unwrap();
    for (role, text) in turns {
        session.push_message_for_test(role, text);
    }
    config.session = Some(session);
    Arc::new(RwLock::new(config))
}

/// Install a mock summarizer client (returns one canned turn) under the global
/// test lock. Hold the returned guard for the duration of the test.
async fn install_summarizer_mock() -> (Arc<MockClient>, TestStateGuard<'static>) {
    let mock = Arc::new(
        MockClient::builder()
            .add_turn(MockTurnBuilder::new().add_text_chunk("Compacted.").build())
            .build(),
    );
    let guard = TestStateGuard::new(Some(mock.clone())).await;
    (mock, guard)
}

/// True if any system message contains `needle`.
fn system_prompt_contains(messages: &[Message], needle: &str) -> bool {
    messages.iter().any(|m| {
        m.role == MessageRole::System
            && matches!(&m.content, MessageContent::Text(t) if t.contains(needle))
    })
}

/// The single rendered-transcript user message sent to the summarizer.
fn rendered_transcript(messages: &[Message]) -> &str {
    messages
        .iter()
        .find(|m| m.role == MessageRole::User)
        .and_then(|m| match &m.content {
            MessageContent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .expect("rendered transcript user message present")
}

// ── compact_session tests ────────────────────────────────────────────────

/// compact_session (no compaction_agent) must send the session history to
/// the LLM — i.e. the user message from the conversation must appear in
/// the ChatCompletionsData that the mock receives.
#[tokio::test]
async fn test_compact_session_default_includes_session_history() {
    let (mock, _guard) = install_summarizer_mock().await;
    let config = with_session(
        Config {
            data: ConfigData {
                stream: false,
                ..Default::default()
            },
            ..Default::default()
        },
        vec![(
            MessageRole::User,
            "Tell me about the Rust ownership model.".to_string(),
        )],
    );

    Config::compact_session(&config).await.unwrap();

    let history = mock.conversation_history();
    assert_eq!(
        history.conversation_history.len(),
        1,
        "expected exactly one LLM call"
    );
    let messages = &history.conversation_history[0].messages;
    let has_history = messages.iter().any(
        |m| matches!(&m.content, MessageContent::Text(t) if t.contains("Rust ownership model")),
    );
    assert!(
        has_history,
        "session history must be forwarded to the compaction LLM; messages: {messages:?}"
    );
}

/// compact_session with a compaction_agent sends the conversation as a
/// rendered transcript (a single user message), NOT the live session history:
/// `with_session` is false, so the request carries the compaction agent's
/// system prompt plus the transcript as the user message.
#[tokio::test]
async fn test_compact_session_with_compaction_agent_sends_rendered_transcript() {
    let temp = tempfile::TempDir::new().unwrap();
    write_agent(
        &temp.path().join("agents"),
        "my-compactor",
        "---\nmodel: gemini:gemini-3.1-flash-lite\n---\nYou are a specialized compaction agent. Produce a concise summary.\n",
    );
    let config = config_with_agent(
        "main",
        "---\nmodel: gemini:gemini-3.1-flash-lite\ncompaction_agent: my-compactor\n---\nYou are the main agent.",
    );
    let config = with_session(
        config,
        vec![(
            MessageRole::User,
            "Tell me about the Rust ownership model.".to_string(),
        )],
    );
    let (mock, _guard) = install_summarizer_mock().await;
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());

    Config::compact_session(&config).await.unwrap();

    let history = mock.conversation_history();
    assert_eq!(
        history.conversation_history.len(),
        1,
        "expected exactly one LLM call"
    );
    let messages = &history.conversation_history[0].messages;

    // The conversation is forwarded as a single rendered transcript carried in
    // a user message — not as the live session's individual role messages.
    let user_messages: Vec<&str> = messages
        .iter()
        .filter(|m| m.role == MessageRole::User)
        .filter_map(|m| match &m.content {
            MessageContent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        user_messages.len(),
        1,
        "exactly one user message (the rendered transcript) is sent; messages: {messages:?}"
    );
    let transcript = user_messages[0];
    assert!(
        transcript.contains("Rust ownership model"),
        "the rendered transcript must include the conversation content; transcript: {transcript:?}"
    );
    assert!(
        transcript.contains("── user ──"),
        "the user message is a rendered transcript with role labels; transcript: {transcript:?}"
    );
    assert!(
        system_prompt_contains(messages, "specialized compaction agent"),
        "compaction agent's system prompt must be in the messages; messages: {messages:?}"
    );
}

/// compact_session with a package-scoped compaction_agent bare name must
/// resolve to the same-package compactor, not a top-level agent of the same name.
#[tokio::test]
async fn test_compact_session_package_bare_compaction_agent_resolves_within_package() {
    let temp = tempfile::TempDir::new().unwrap();
    let pkg_agents = temp.path().join("packages/mypkg/agents");
    // Use /gemini (leading slash) to escape to top-level so the model reference
    // is not rewritten to mypkg/gemini by apply_package_agent_transforms.
    write_agent(
        &pkg_agents,
        "compactor",
        "---\nmodel: /gemini:gemini-3.1-flash-lite\n---\nYou are the PACKAGE compactor. Produce a concise summary.\n",
    );
    std::fs::write(
        temp.path().join("packages/mypkg/manifest.yaml"),
        "name: mypkg\nsource:\n  type: git\n  url: file:///fake\n  tag: v1.0.0\n  commit: abc123\ninstalled_at: \"2025-01-01T00:00:00Z\"\n",
    )
    .unwrap();
    write_agent(
        &temp.path().join("agents"),
        "compactor",
        "---\nmodel: gemini:gemini-3.1-flash-lite\n---\nYou are the TOP-LEVEL compactor. Produce a concise summary.\n",
    );

    let mut config = config_with_agent(
        "mypkg/main",
        "---\nmodel: gemini:gemini-3.1-flash-lite\n---\nYou are the main package agent.",
    );
    config
        .agent
        .as_mut()
        .unwrap()
        .set_compaction_agent(Some("compactor".to_string()));
    let config = with_session(
        config,
        vec![(
            MessageRole::User,
            "Tell me about package-local compaction.".to_string(),
        )],
    );
    let (mock, _guard) = install_summarizer_mock().await;
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());

    Config::compact_session(&config).await.unwrap();

    let history = mock.conversation_history();
    assert_eq!(
        history.conversation_history.len(),
        1,
        "expected exactly one LLM call"
    );
    let messages = &history.conversation_history[0].messages;
    assert!(
        system_prompt_contains(messages, "PACKAGE compactor"),
        "package compactor system prompt must be used; messages: {messages:?}"
    );
    assert!(
        !system_prompt_contains(messages, "TOP-LEVEL compactor"),
        "top-level compactor system prompt must not be used; messages: {messages:?}"
    );
}

/// compact_session must honor a non-default `compaction_keep_recent_turns`
/// from the compaction agent's frontmatter: a smaller value keeps fewer recent
/// turns verbatim and therefore compacts MORE of the conversation. With
/// `compaction_keep_recent_turns: 1` and four user turns, only turn 4 is kept
/// verbatim, so the rendered transcript carries turns 1–3 (the default of 3
/// would keep turns 2–4 and render only turn 1).
#[tokio::test]
async fn test_compact_session_honors_compaction_keep_recent_turns() {
    let temp = tempfile::TempDir::new().unwrap();
    write_agent(
        &temp.path().join("agents"),
        "my-compactor",
        "---\nmodel: gemini:gemini-3.1-flash-lite\ncompaction_keep_recent_turns: 1\n---\nProduce a concise summary.\n",
    );
    let config = config_with_agent(
        "main",
        "---\nmodel: gemini:gemini-3.1-flash-lite\ncompaction_agent: my-compactor\n---\nMain agent.",
    );
    // Short messages so the turn-count limit (not the token budget) governs split.
    let mut turns = Vec::new();
    for i in 1..=4 {
        turns.push((MessageRole::User, format!("user turn {i}")));
        turns.push((MessageRole::Assistant, format!("assistant reply {i}")));
    }
    let config = with_session(config, turns);
    let (mock, _guard) = install_summarizer_mock().await;
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());

    Config::compact_session(&config).await.unwrap();

    // The single summarizer call receives a transcript covering turns 1–3.
    let history = mock.conversation_history();
    assert_eq!(
        history.conversation_history.len(),
        1,
        "expected one LLM call"
    );
    let transcript = rendered_transcript(&history.conversation_history[0].messages);

    // Exactly three user turns compacted (default keep of 3 would compact one),
    // and turn 4 stays out of the transcript because it is kept verbatim.
    let compacted_turns = transcript.matches("── user ──").count();
    assert_eq!(
        compacted_turns, 3,
        "keep_recent_turns=1 must compact 3 of 4 turns; transcript: {transcript:?}"
    );
    assert!(
        !transcript.contains("user turn 4"),
        "the last turn must be kept verbatim, not compacted; transcript: {transcript:?}"
    );

    // The kept-verbatim suffix lands back as [U4, A4]; summary stays on runtime field.
    let guard = config.read();
    let session = guard.session.as_ref().unwrap();
    assert_eq!(
        session.messages.len(),
        2,
        "expected one kept turn (2 messages)"
    );
    let summary = session
        .compaction_summary
        .as_deref()
        .expect("expected runtime compaction summary to be populated");
    assert!(!summary.is_empty(), "expected non-empty compaction summary");
    assert!(
        summary.contains("Compacted"),
        "expected runtime compaction summary to contain 'Compacted', got {summary:?}"
    );
}

#[tokio::test]
async fn test_apply_compaction_summary_skips_swapped_session() {
    let config = with_session(
        Config {
            data: ConfigData {
                stream: false,
                ..Default::default()
            },
            ..Default::default()
        },
        vec![
            (MessageRole::User, "first prompt".to_string()),
            (MessageRole::Assistant, "first reply".to_string()),
            (MessageRole::User, "second prompt".to_string()),
            (MessageRole::Assistant, "second reply".to_string()),
        ],
    );
    let original_session = config.read().session.as_ref().unwrap().clone();
    let original_id = original_session.id.clone();
    let original_message_len = original_session.messages.len();
    let original_continuous = config
        .read()
        .last_message
        .as_ref()
        .map(|last| last.continuous);
    let split = 2;

    let mut swapped_session = original_session.clone();
    swapped_session.id = "swapped-session".to_string();
    config.write().session = Some(swapped_session);

    let applied =
        Config::apply_compaction_summary(&config, &original_id, "summary".to_string(), split);

    let guard = config.read();
    let active = guard.session.as_ref().unwrap();
    assert!(!applied, "session swap must skip stale compaction result");
    assert_eq!(active.id, "swapped-session");
    assert_eq!(active.messages.len(), original_message_len);
    assert_eq!(
        guard.last_message.as_ref().map(|last| last.continuous),
        original_continuous
    );
}
