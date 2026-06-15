//! compact_session / compaction-agent tests for the config module.
#![cfg(test)]

use super::test_support::EnvGuard;
use super::*;
use harnx_core::message::{MessageContent, MessageRole};

// ── compact_session tests ────────────────────────────────────────────────

/// Helper: create a GlobalConfig with a session that already has one user
/// message in it, suitable for compaction tests.
fn make_config_with_session() -> GlobalConfig {
    let mut config = Config {
        data: ConfigData {
            stream: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut session = self::session::new(&config, "test-session").unwrap();
    session.push_message_for_test(
        MessageRole::User,
        "Tell me about the Rust ownership model.".to_string(),
    );
    config.session = Some(session);
    Arc::new(RwLock::new(config))
}

/// compact_session (no compaction_agent) must send the session history to
/// the LLM — i.e. the user message from the conversation must appear in
/// the ChatCompletionsData that the mock receives.
#[tokio::test]
async fn test_compact_session_default_includes_session_history() {
    use crate::client::TestStateGuard;
    use crate::test_utils::{MockClient, MockTurnBuilder};

    let mock = Arc::new(
        MockClient::builder()
            .add_turn(MockTurnBuilder::new().add_text_chunk("Summary.").build())
            .build(),
    );
    let _guard = TestStateGuard::new(Some(mock.clone())).await;
    let config = make_config_with_session();

    Config::compact_session(&config).await.unwrap();

    let history = mock.conversation_history();
    assert_eq!(
        history.conversation_history.len(),
        1,
        "expected exactly one LLM call"
    );
    let messages = &history.conversation_history[0].messages;
    let has_history = messages.iter().any(|m| {
        if let MessageContent::Text(t) = &m.content {
            t.contains("Rust ownership model")
        } else {
            false
        }
    });
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
    use crate::client::TestStateGuard;
    use crate::test_utils::{MockClient, MockTurnBuilder};
    use std::io::Write as _;

    // Write a minimal compaction agent file to a temp dir and point the
    // config's agents directory at it via HARNX_CONFIG_DIR.
    let temp = tempfile::TempDir::new().unwrap();
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();

    let agent_content = "---\nmodel: gemini:gemini-3.1-flash-lite\n---\nYou are a specialized compaction agent. Produce a concise summary.\n";
    let mut f = std::fs::File::create(agents_dir.join("my-compactor.md")).unwrap();
    f.write_all(agent_content.as_bytes()).unwrap();

    // Build a config where the current (non-session) agent has
    // compaction_agent = "my-compactor".
    let mut main_agent = Agent::new(AgentConfig::from_markdown(
            "main",
            "---\nmodel: gemini:gemini-3.1-flash-lite\ncompaction_agent: my-compactor\n---\nYou are the main agent.",
        ).unwrap());
    main_agent.set_model(crate::client::Model::new("gemini", "gemini-3.1-flash-lite"));

    let mut config = Config {
        data: ConfigData {
            stream: false,
            ..Default::default()
        },
        ..Default::default()
    };
    // Point Config::agent_file() at the temp dir via HARNX_CONFIG_DIR.
    // Use an RAII guard so the env var is restored even on panic.  The
    // guard is created *after* `TestStateGuard` acquires the global test
    // lock so concurrent tests cannot race on the env var.
    config.agent = Some(main_agent);

    let mut session = self::session::new(&config, "test-session").unwrap();
    session.push_message_for_test(
        MessageRole::User,
        "Tell me about the Rust ownership model.".to_string(),
    );
    config.session = Some(session);
    let config = Arc::new(RwLock::new(config));

    let mock = Arc::new(
        MockClient::builder()
            .add_turn(MockTurnBuilder::new().add_text_chunk("Compacted.").build())
            .build(),
    );
    let _guard = TestStateGuard::new(Some(mock.clone())).await;
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
    // The transcript is the flattened, role-labeled rendering (not raw live
    // session messages), so it carries the render_transcript role markers.
    assert!(
        transcript.contains("── user ──"),
        "the user message is a rendered transcript with role labels; transcript: {transcript:?}"
    );

    // The compaction agent's system prompt must also be present.
    let has_system = messages.iter().any(|m| {
        m.role == MessageRole::System
            && if let MessageContent::Text(t) = &m.content {
                t.contains("specialized compaction agent")
            } else {
                false
            }
    });
    assert!(
        has_system,
        "compaction agent's system prompt must be in the messages; messages: {messages:?}"
    );
}

/// compact_session with a package-scoped compaction_agent bare name must
/// resolve to the same-package compactor, not a top-level agent of the same name.
#[tokio::test]
async fn test_compact_session_package_bare_compaction_agent_resolves_within_package() {
    use crate::client::TestStateGuard;
    use crate::test_utils::{MockClient, MockTurnBuilder};
    use std::io::Write as _;

    let temp = tempfile::TempDir::new().unwrap();
    let package_agents_dir = temp.path().join("packages/mypkg/agents");
    std::fs::create_dir_all(&package_agents_dir).unwrap();
    let top_level_agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&top_level_agents_dir).unwrap();

    std::fs::write(
            temp.path().join("packages/mypkg/manifest.yaml"),
            "name: mypkg\nsource:\n  type: git\n  url: file:///fake\n  tag: v1.0.0\n  commit: abc123\ninstalled_at: \"2025-01-01T00:00:00Z\"\n",
        )
        .unwrap();

    // Use /gemini (leading slash) to escape to top-level so the model
    // reference is not rewritten to mypkg/gemini by apply_package_agent_transforms.
    let package_compactor = "---\nmodel: /gemini:gemini-3.1-flash-lite\n---\nYou are the PACKAGE compactor. Produce a concise summary.\n";
    let mut package_file = std::fs::File::create(package_agents_dir.join("compactor.md")).unwrap();
    package_file
        .write_all(package_compactor.as_bytes())
        .unwrap();

    let top_level_compactor = "---\nmodel: gemini:gemini-3.1-flash-lite\n---\nYou are the TOP-LEVEL compactor. Produce a concise summary.\n";
    let mut top_level_file =
        std::fs::File::create(top_level_agents_dir.join("compactor.md")).unwrap();
    top_level_file
        .write_all(top_level_compactor.as_bytes())
        .unwrap();

    let mut main_agent = Agent::new(
        AgentConfig::from_markdown(
            "mypkg/main",
            "---\nmodel: gemini:gemini-3.1-flash-lite\n---\nYou are the main package agent.",
        )
        .unwrap(),
    );
    main_agent.set_compaction_agent(Some("compactor".to_string()));
    main_agent.set_model(crate::client::Model::new("gemini", "gemini-3.1-flash-lite"));

    let mut config = Config {
        data: ConfigData {
            stream: false,
            ..Default::default()
        },
        ..Default::default()
    };
    config.agent = Some(main_agent);

    let mut session = self::session::new(&config, "test-session").unwrap();
    session.push_message_for_test(
        MessageRole::User,
        "Tell me about package-local compaction.".to_string(),
    );
    config.session = Some(session);
    let config = Arc::new(RwLock::new(config));

    let mock = Arc::new(
        MockClient::builder()
            .add_turn(MockTurnBuilder::new().add_text_chunk("Compacted.").build())
            .build(),
    );
    let _guard = TestStateGuard::new(Some(mock.clone())).await;
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());

    Config::compact_session(&config).await.unwrap();

    let history = mock.conversation_history();
    assert_eq!(
        history.conversation_history.len(),
        1,
        "expected exactly one LLM call"
    );
    let messages = &history.conversation_history[0].messages;

    let has_package_system = messages.iter().any(|m| {
        m.role == MessageRole::System
            && if let MessageContent::Text(t) = &m.content {
                t.contains("PACKAGE compactor")
            } else {
                false
            }
    });
    assert!(
        has_package_system,
        "package compactor system prompt must be used; messages: {messages:?}"
    );

    let has_top_level_system = messages.iter().any(|m| {
        m.role == MessageRole::System
            && if let MessageContent::Text(t) = &m.content {
                t.contains("TOP-LEVEL compactor")
            } else {
                false
            }
    });
    assert!(
        !has_top_level_system,
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
    use crate::client::TestStateGuard;
    use crate::test_utils::{MockClient, MockTurnBuilder};
    use std::io::Write as _;

    let temp = tempfile::TempDir::new().unwrap();
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let mut f = std::fs::File::create(agents_dir.join("my-compactor.md")).unwrap();
    f.write_all(b"---\nmodel: gemini:gemini-3.1-flash-lite\ncompaction_keep_recent_turns: 1\n---\nProduce a concise summary.\n").unwrap();

    let mut main_agent = Agent::new(
        AgentConfig::from_markdown(
            "main",
            "---\nmodel: gemini:gemini-3.1-flash-lite\ncompaction_agent: my-compactor\n---\nMain agent.",
        )
        .unwrap(),
    );
    main_agent.set_model(crate::client::Model::new("gemini", "gemini-3.1-flash-lite"));

    let mut config = Config {
        data: ConfigData {
            stream: false,
            ..Default::default()
        },
        ..Default::default()
    };
    config.agent = Some(main_agent);

    // Short messages so the turn-count limit (not the token budget) governs split.
    let mut session = self::session::new(&config, "test-session").unwrap();
    for i in 1..=4 {
        session.push_message_for_test(MessageRole::User, format!("user turn {i}"));
        session.push_message_for_test(MessageRole::Assistant, format!("assistant reply {i}"));
    }
    config.session = Some(session);
    let config = Arc::new(RwLock::new(config));

    let mock = Arc::new(
        MockClient::builder()
            .add_turn(MockTurnBuilder::new().add_text_chunk("Compacted.").build())
            .build(),
    );
    let _guard = TestStateGuard::new(Some(mock.clone())).await;
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());

    Config::compact_session(&config).await.unwrap();

    // The single summarizer call receives a transcript covering turns 1–3.
    let history = mock.conversation_history();
    assert_eq!(
        history.conversation_history.len(),
        1,
        "expected one LLM call"
    );
    let transcript = history.conversation_history[0]
        .messages
        .iter()
        .find(|m| m.role == MessageRole::User)
        .and_then(|m| match &m.content {
            MessageContent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .expect("rendered transcript user message present");

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

    // The kept-verbatim suffix lands back as [summary_system, U4, A4].
    let kept = config.read().session.as_ref().unwrap().messages.len();
    assert_eq!(
        kept, 3,
        "expected the summary plus the one kept turn (2 messages)"
    );
}
