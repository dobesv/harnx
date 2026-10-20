//! Static handoff transcript item tests.
//!
//! Tests that:
//! - HandoffSession renders as a cyan row "@ {agent} [{short_id}]"
//! - Enter on focused HandoffSession opens the target session
//! - Restored history produces the same single row

use super::{create_agent_stubs, test_config, test_config_with_mock_client_and_agent};
use crate::test_utils::{TestEnvironment, TuiTestHarness, ENV_LOCK};
use crate::types::TranscriptItem;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_runtime::config::GlobalConfig;
use tempfile::TempDir;

async fn cleared_handoff_harness(config: GlobalConfig) -> TuiTestHarness {
    let mut harness = TuiTestHarness::with_config(config).await;
    harness.tui().clear_transcript();
    harness
}

fn handoff_output(extra: serde_json::Value) -> serde_json::Value {
    let mut output = serde_json::json!({
        "content": [{"type": "text", "text": "Handing off to pantheon/atlas; target session: abc123."}],
    });
    output
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    output["handoff"] = serde_json::json!({ "agent": "pantheon/atlas", "session_id": "abc123" });
    output
}

fn handoff_item(agent: &str, session_id: &str) -> TranscriptItem {
    TranscriptItem::HandoffSession {
        agent: agent.to_string(),
        session_id: session_id.to_string(),
    }
}

fn assert_focused_handoff(harness: &mut TuiTestHarness, expected_agent: &str, expected_id: &str) {
    let focus = harness.tui().app.transcript_focus.unwrap();
    match harness.tui().app.transcript.get(focus) {
        Some(TranscriptItem::HandoffSession { agent, session_id }) => {
            assert_eq!(agent, expected_agent);
            assert_eq!(session_id, expected_id);
        }
        other => panic!("expected focused HandoffSession, got {other:?}"),
    }
}

fn assert_handoff_screen(screen: &str, expected_row: &str) {
    let (row, rest) = screen.split_once('\n').expect("screen has multiple rows");
    assert_eq!(row, format!("{expected_row:<80}"));
    insta::allow_duplicates! {
        insta::assert_snapshot!(rest, @"
                                                                                    
                                                                                    
                                                                                    
                                                                                    
                                                                                    
                                                                                    
                                                                                    
                                                                                    
                                                                                    
                                                                                    
                                                                                    
                                                                                    
                                                                                    
                                                                                    
                                                                                    
                                                                                    
                                                                                    
                                                                                    
                                                                                    
                                                                                    
    • Input
    ");
    }
}

#[tokio::test]
async fn handoff_session_renders_as_cyan_row() {
    let config = test_config();
    let mut harness = cleared_handoff_harness(config).await;
    harness
        .tui()
        .app
        .transcript
        .push(handoff_item("pantheon/atlas", "abc123xyz"));

    harness.render();
    let screen = harness.screen_contents();
    assert_handoff_screen(&screen, "  ↪ handoff @ pantheon/atlas [abc123xy]");
}

#[tokio::test]
async fn handoff_session_truncates_long_session_id() {
    let config = test_config();
    let mut harness = cleared_handoff_harness(config).await;
    harness.tui().app.transcript.push(handoff_item(
        "pantheon/atlas",
        "very-long-session-id-12345678",
    ));

    harness.render();
    let screen = harness.screen_contents();
    // Short ID should be first 8 chars
    assert_handoff_screen(&screen, "  ↪ handoff @ pantheon/atlas [very-lon]");
}

#[tokio::test]
async fn handoff_session_is_focusable_and_navigable() {
    let config = test_config();
    let mut harness = cleared_handoff_harness(config).await;
    harness.tui().app.transcript.push(TranscriptItem::UserText {
        text: "First".to_string(),
        seq: None,
        timestamp: None,
    });
    harness
        .tui()
        .app
        .transcript
        .push(handoff_item("pantheon/atlas", "abc123"));

    // The transcript now has UserText at index 0, HandoffSession at index 1.
    // Both are navigable. Press Up once on blank input should focus
    // the last navigable item (HandoffSession at index 1).
    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();

    harness.render();
    assert!(
        harness.tui().app.transcript_focus.is_some(),
        "expected focus after Up on blank input"
    );
    assert_focused_handoff(&mut harness, "pantheon/atlas", "abc123");
}

#[tokio::test]
async fn handoff_from_tool_result_in_transcript() {
    let config = test_config();
    let mut harness = cleared_handoff_harness(config).await;

    // Add a handoff result directly via tool_completed_to_transcript_items logic
    let output = handoff_output(serde_json::json!({}));

    let items = crate::tool_transcript::tool_completed_to_transcript_items(&output, None);
    for item in items {
        harness.tui().app.transcript.push(item);
    }

    harness.render();
    let screen = harness.screen_contents();
    assert_handoff_screen(&screen, "  ↪ handoff @ pantheon/atlas [abc123]");
}

#[tokio::test]
async fn handoff_session_row_in_history_matches_live() {
    use harnx_core::message::{Message, MessageContent, MessageContentToolCalls, MessageRole};
    use harnx_core::tool::{ToolCall, ToolResult};

    let config = test_config_with_mock_client_and_agent("source", Some("parent-session"));
    let output = handoff_output(serde_json::json!({
        "action": "switch_agent", "agent": "pantheon/atlas",
    }));
    let live_items = crate::tool_transcript::tool_completed_to_transcript_items(&output, None);
    let result = ToolResult {
        call: ToolCall {
            name: "atlas_session_handoff".into(),
            arguments: serde_json::json!({"prompt": "Continue"}),
            id: Some("handoff-call".into()),
            thought_signature: None,
            reasoning_provenance: None,
        },
        output,
        markdown: None,
        content: vec![],
        switch_agent: None,
        execution_context: None,
    };
    config
        .write()
        .session
        .as_mut()
        .unwrap()
        .messages
        .push(Message::new(
            MessageRole::Tool,
            MessageContent::ToolCalls(MessageContentToolCalls::new(
                vec![result],
                String::new(),
                None,
            )),
        ));
    let history_items = crate::lifecycle::session_history_transcript_items(&config).await;
    let history_rows: Vec<_> = history_items
        .iter()
        .filter(|item| matches!(item, TranscriptItem::HandoffSession { .. }))
        .collect();
    assert_eq!(live_items.len(), 1);
    assert_eq!(
        history_rows.len(),
        1,
        "restored history must contain one handoff row"
    );
    assert_eq!(
        crate::types::Tui::get_transcript_item_text(&live_items[0]),
        crate::types::Tui::get_transcript_item_text(history_rows[0]),
    );
    assert!(!history_items
        .iter()
        .any(|item| matches!(item, TranscriptItem::SubAgentSession { .. })));
}

/// Test that Enter on a focused HandoffSession row opens the target session.
/// This test verifies the navigation logic and that the TUI state is reconciled.
#[tokio::test]
async fn handoff_session_enter_opens_target_session() {
    let temp = TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());

    // Create agent stub directories for source and target (must match agent name)
    create_agent_stubs(&temp.path().join("agents"), &["target"]);

    let config = test_config_with_mock_client_and_agent("source", Some("source-session"));

    let mut harness = cleared_handoff_harness(config.clone()).await;

    // Push a handoff session item
    harness
        .tui()
        .app
        .transcript
        .push(handoff_item("target", "target-session-abc"));

    // Focus the handoff item (it's the only one, so one Up is enough)
    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();

    // Verify focus is on the handoff item
    assert_focused_handoff(&mut harness, "target", "target-session-abc");

    // Press Enter to open the target session
    harness
        .tui()
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();

    // Verify the config now points to the target agent + session
    {
        let cfg = config.read();
        assert_eq!(
            cfg.agent.as_ref().map(|a| a.name()),
            Some("target"),
            "config agent should be target"
        );
        assert_eq!(
            cfg.session.as_ref().map(|s| s.id()),
            Some("target-session-abc"),
            "config session ID should be target"
        );
    }

    // Verify TUI state was reconciled for the new session
    assert!(
        harness.tui().app.transcript_focus.is_none(),
        "transcript focus should be cleared after handoff"
    );
    assert!(
        !harness.tui().app.transcript_browsing,
        "transcript browsing should be cleared after handoff"
    );
}

#[tokio::test]
async fn handoff_session_enter_while_busy_keeps_active_session() {
    let temp = TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    create_agent_stubs(&temp.path().join("agents"), &["target"]);

    for browsing in [false, true] {
        let config = test_config_with_mock_client_and_agent("source", Some("source-session"));
        let mut harness = TuiTestHarness::with_config(config.clone()).await;
        let tui = harness.tui();
        tui.clear_transcript();
        tui.app
            .transcript
            .push(handoff_item("target", "target-session-abc"));
        tui.app.llm_busy = true;
        tui.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(tui.app.transcript_focus, Some(0));
        tui.app.transcript_browsing = browsing;

        tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();

        let cfg = config.read();
        assert_eq!(cfg.agent.as_ref().map(|agent| agent.name()), Some("source"));
        assert_eq!(
            cfg.session.as_ref().map(|session| session.id()),
            Some("source-session")
        );
        assert!(tui.app.llm_busy);
        assert_eq!(tui.app.transcript_focus, Some(0));
        assert_eq!(tui.app.transcript_browsing, browsing);
        assert!(tui.app.subagent_view_stack.is_empty());
        assert!(matches!(
            tui.app.transcript.last(),
            Some(TranscriptItem::StatusLine(message))
                if message == "Cannot open handoff session while a turn is running."
        ));
    }
}

async fn seed_handoff_child_transcript(config: &GlobalConfig) {
    use harnx_core::message::{MessageContent, MessageRole};
    use harnx_core::session::SessionLogEntry;
    use harnx_runtime::nats_session_log::NatsSessionLog;
    use harnx_runtime::nats_session_metadata::{SessionMetadata, SessionMetadataStore};

    use harnx_runtime::config::LOCAL_CLUSTER_KEY;

    let server = harnx_runtime::config::resolve_local_nats_server_config()
        .await
        .unwrap();
    config.write().nats_servers.push(server);
    let nats_config = config.read().clone();
    let jetstream = nats_config.nats_jetstream(LOCAL_CLUSTER_KEY).await.unwrap();
    let metadata = SessionMetadataStore::ensure(&jetstream, 1).await.unwrap();
    metadata
        .create(&SessionMetadata::new(
            "target-session-abc",
            harnx_runtime::SessionInitializer::named("target", Default::default()),
        ))
        .await
        .unwrap();
    let log = NatsSessionLog::for_agent(jetstream, "target", "target-session-abc").with_replicas(1);
    log.append_event_async(&SessionLogEntry::Message {
        id: None,
        timestamp: None,
        fence_token: None,
        role: MessageRole::Assistant,
        content: MessageContent::Text("Child session reply".into()),
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handoff_session_enter_loads_child_transcript() {
    use harnx_runtime::config::LOCAL_CLUSTER_KEY;

    let temp = TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let config = test_config_with_mock_client_and_agent("source", Some("source-session"));
    seed_handoff_child_transcript(&config).await;

    let mut harness = TuiTestHarness::with_config(config.clone()).await;
    let tui = harness.tui();
    tui.clear_transcript();
    tui.app.transcript.push(handoff_item(
        &format!("target@{LOCAL_CLUSTER_KEY}"),
        "target-session-abc",
    ));
    tui.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(tui.app.transcript_focus, Some(0));
    tui.app.streaming_open = true;
    tui.app.main_streamed_text_idx = Some(0);
    tui.app.streamed_text_idx = Some(0);
    tui.app.transcript_selection_anchor = Some(0);
    tui.app.scroll_state.follow = false;

    tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();

    {
        let cfg = config.read();
        assert_eq!(
            cfg.active_agent_ref(),
            Some(format!("target@{LOCAL_CLUSTER_KEY}"))
        );
        assert_eq!(cfg.session.as_ref().unwrap().id(), "target-session-abc");
    }
    assert!(
        tui.app.transcript.iter().any(|item| matches!(
            item, TranscriptItem::AssistantText { text, .. } if text == "Child session reply"
        )),
        "child transcript must replace parent transcript: {:?}",
        tui.app.transcript
    );
    assert!(!tui
        .app
        .transcript
        .iter()
        .any(|item| matches!(item, TranscriptItem::HandoffSession { .. })));
    assert_saved_handoff_navigation_state(tui);

    harness.render();
    let screen = harness.screen_contents();
    assert!(screen.contains("Child session reply"));
    assert!(!screen.contains("↪ handoff"));
}

fn assert_saved_handoff_navigation_state(tui: &crate::types::Tui) {
    assert!(!tui.app.streaming_open);
    assert!(tui.app.main_streamed_text_idx.is_none());
    assert!(tui.app.streamed_text_idx.is_none());
    assert!(tui.app.transcript_focus.is_none());
    assert!(tui.app.transcript_selection_anchor.is_none());
    assert!(!tui.app.transcript_browsing);
    assert!(!tui.app.detail_view_open);
    assert!(tui.app.scroll_state.follow);
    assert_eq!(
        tui.session_activity_target,
        tui.session_activity_destination()
    );
    assert!(tui.session_activity_handle.is_some());
    assert!(
        !tui.app.llm_busy,
        "opening a saved handoff must not start a turn"
    );
    assert!(tui.current_prompt_abort.is_none());
    assert!(tui.active_remote_session.is_none());
}

#[tokio::test]
async fn handoff_session_insert_preserves_full_identity() {
    let mut harness = TuiTestHarness::with_config(test_config()).await;
    let tui = harness.tui();
    tui.clear_transcript();
    let item = handoff_item("pantheon/atlas", "very-long-session-id-12345678");
    let expected = "@ pantheon/atlas [very-long-session-id-12345678]";
    assert_eq!(
        crate::types::Tui::get_transcript_item_text(&item).as_deref(),
        Some(expected)
    );
    tui.app.transcript.push(item);
    tui.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();
    tui.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(tui.app.input.lines(), [expected]);
    assert!(tui.app.transcript_focus.is_none());
}

#[tokio::test]
async fn handoff_session_short_id_is_utf8_safe() {
    let mut harness = TuiTestHarness::with_config(test_config()).await;
    harness.tui().clear_transcript();
    // Byte 8 lies inside the first multibyte character.
    harness
        .tui()
        .app
        .transcript
        .push(handoff_item("atlas", "1234567é日child"));
    harness.render();
    assert!(harness
        .screen_contents()
        .contains("↪ handoff @ atlas [1234567é]"));
}
