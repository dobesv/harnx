use super::{
    create_agent_stubs, normalize_screen, test_config, test_config_with_mock_client_and_agent,
};
use crate::test_utils::{TestEnvironment, TuiTestHarness, ENV_LOCK};
use crate::types::{MonitoredSessionKey, SubAgentStatus, TranscriptItem, TuiEvent};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent, ToolEvent, TurnEvent};
use std::time::Duration;

fn monitored_key(agent: &str, session_id: &str) -> MonitoredSessionKey {
    MonitoredSessionKey {
        cluster: harnx_runtime::config::LOCAL_CLUSTER_KEY.to_string(),
        agent: agent.to_string(),
        session_id: session_id.to_string(),
    }
}

fn assistant_text(text: &str) -> TranscriptItem {
    TranscriptItem::AssistantText {
        text: text.to_string(),
        seq: None,
        timestamp: None,
        rendered_cache: None,
    }
}

async fn emit_subagent_started(tui: &mut crate::types::Tui, key: &MonitoredSessionKey) {
    tui.handle_tui_event(TuiEvent::Agent(AgentEvent::Turn(
        TurnEvent::SubAgentStarted {
            agent: key.agent.clone(),
            session_id: key.session_id.clone(),
        },
    )))
    .await
    .unwrap();
}

fn completed_subagent_event(key: &MonitoredSessionKey) -> AgentEvent {
    AgentEvent::Tool(ToolEvent::Completed {
        id: "delegate-call".into(),
        output: serde_json::json!({
            "content": [{"type": "text", "text": "child response"}],
            "sub_agent": {
                "agent": key.agent,
                "session_id": key.session_id,
            }
        }),
        markdown: Some("child response".into()),
    })
}

#[tokio::test]
async fn compact_subagent_row_deduplicates_durable_completion() {
    let mut harness = TuiTestHarness::with_size(60, 12).await;
    harness.tui().clear_transcript();
    let key = monitored_key("researcher", "01234567abcdef");
    emit_subagent_started(harness.tui(), &key).await;

    assert!(matches!(
        harness.tui().app.transcript.as_slice(),
        [TranscriptItem::SubAgentSession { key: row_key, status: SubAgentStatus::Running }]
            if row_key == &key
    ));
    assert_eq!(harness.tui().subagent_monitor_handles.len(), 1);
    harness.render();
    insta::assert_snapshot!(
        "compact_subagent_session_running",
        normalize_screen(&harness.screen_contents())
    );

    let completed = completed_subagent_event(&key);
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(completed.clone()))
        .await
        .unwrap();
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(completed))
        .await
        .unwrap();

    assert!(matches!(
        harness.tui().app.transcript.as_slice(),
        [TranscriptItem::SubAgentSession { key: row_key, status: SubAgentStatus::Completed }]
            if row_key == &key
    ));
}

#[tokio::test]
async fn prompting_a_completed_child_restarts_monitoring_and_tracks_failure() {
    let mut harness = TuiTestHarness::new().await;
    harness.tui().clear_transcript();
    let key = monitored_key("researcher", "repeat-session");
    emit_subagent_started(harness.tui(), &key).await;
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(completed_subagent_event(&key)))
        .await
        .unwrap();
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(AgentEvent::Tool(ToolEvent::Started {
            id: "delegate-call-2".into(),
            name: "researcher_session_prompt".into(),
            kind: harnx_core::event::ToolKind::Other,
            markdown: None,
            input: serde_json::json!({"session_id": key.session_id}),
            locations: vec![],
        })))
        .await
        .unwrap();
    emit_subagent_started(harness.tui(), &key).await;

    let statuses = harness
        .tui()
        .app
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::SubAgentSession { status, .. } => Some(status),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        statuses,
        vec![&SubAgentStatus::Completed, &SubAgentStatus::Running]
    );
    harness
        .tui()
        .handle_tui_event(TuiEvent::SubAgentSessionEvent {
            key: key.clone(),
            event: AgentEvent::Model(ModelEvent::Error("child failed".into())),
        })
        .await
        .unwrap();
    assert_eq!(
        harness.tui().app.monitored_sessions[&key].status,
        SubAgentStatus::Failed
    );
}

async fn nested_session_harness() -> (TuiTestHarness, MonitoredSessionKey, MonitoredSessionKey) {
    let mut harness = TuiTestHarness::with_size(64, 14).await;
    harness.tui().clear_transcript();
    let parent = monitored_key("researcher", "parent-session-123");
    let nested = monitored_key("fact-checker", "nested-session-456");
    emit_subagent_started(harness.tui(), &parent).await;
    harness
        .tui()
        .handle_tui_event(TuiEvent::SubAgentSessionSnapshot {
            key: parent.clone(),
            transcript: vec![
                assistant_text("parent transcript"),
                TranscriptItem::SubAgentSession {
                    key: nested.clone(),
                    status: SubAgentStatus::Running,
                },
            ],
            status: SubAgentStatus::Running,
        })
        .await
        .unwrap();
    harness
        .tui()
        .handle_tui_event(TuiEvent::SubAgentSessionSnapshot {
            key: nested.clone(),
            transcript: vec![assistant_text("nested transcript")],
            status: SubAgentStatus::Completed,
        })
        .await
        .unwrap();
    (harness, parent, nested)
}

async fn press_key(harness: &mut TuiTestHarness, code: KeyCode) {
    harness
        .tui()
        .handle_key(KeyEvent::new(code, KeyModifiers::NONE))
        .await
        .unwrap();
}

async fn open_focused_root_subagent(harness: &mut TuiTestHarness) {
    press_key(harness, KeyCode::Up).await;
    assert!(
        harness.tui().app.subagent_view_stack.is_empty(),
        "focusing a child row must not open it"
    );
    press_key(harness, KeyCode::Enter).await;
}

#[tokio::test]
async fn focused_subagent_row_opens_fullscreen_only_after_enter() {
    let (mut harness, parent, _) = nested_session_harness().await;
    open_focused_root_subagent(&mut harness).await;
    assert_eq!(harness.tui().app.subagent_view_stack, vec![parent]);
    harness.render();
    insta::assert_snapshot!(
        "subagent_session_fullscreen",
        normalize_screen(&harness.screen_contents())
    );
}

#[tokio::test]
async fn fullscreen_subagent_enter_drills_into_nested_session_and_returns() {
    let (mut harness, parent, nested) = nested_session_harness().await;
    open_focused_root_subagent(&mut harness).await;
    press_key(&mut harness, KeyCode::Up).await;
    assert_eq!(harness.tui().app.subagent_view_stack, vec![parent.clone()]);
    press_key(&mut harness, KeyCode::Enter).await;
    assert_eq!(
        harness.tui().app.subagent_view_stack,
        vec![parent.clone(), nested.clone()]
    );
    press_key(&mut harness, KeyCode::PageUp).await;
    assert!(!harness.tui().app.monitored_sessions[&nested].scroll.follow);
    press_key(&mut harness, KeyCode::Esc).await;
    assert_eq!(harness.tui().app.subagent_view_stack, vec![parent]);
    press_key(&mut harness, KeyCode::Esc).await;
    assert!(harness.tui().app.subagent_view_stack.is_empty());
}

#[tokio::test]
async fn fullscreen_subagent_enter_opens_focused_entry_detail() {
    let (mut harness, parent, _) = nested_session_harness().await;
    open_focused_root_subagent(&mut harness).await;
    press_key(&mut harness, KeyCode::Down).await;
    assert_eq!(
        harness.tui().app.monitored_sessions[&parent].transcript_focus,
        Some(0)
    );

    press_key(&mut harness, KeyCode::Enter).await;
    assert!(harness.tui().app.detail_view_open);
    assert!(matches!(
        harness.tui().app.detail_view_entry.as_ref(),
        Some(TranscriptItem::AssistantText { text, .. }) if text == "parent transcript"
    ));
    harness.render();
    assert!(harness.screen_contents().contains("parent transcript"));

    press_key(&mut harness, KeyCode::Esc).await;
    assert!(!harness.tui().app.detail_view_open);
    assert_eq!(harness.tui().app.subagent_view_stack, vec![parent]);
}

#[tokio::test]
async fn fullscreen_subagent_navigation_is_bounded_and_keeps_focus_visible() {
    let mut harness = TuiTestHarness::with_size(48, 8).await;
    harness.tui().clear_transcript();
    let child = monitored_key("researcher", "long-child-session");
    emit_subagent_started(harness.tui(), &child).await;
    let transcript = (0..12)
        .map(|index| assistant_text(&format!("child row {index}")))
        .collect();
    harness
        .tui()
        .handle_tui_event(TuiEvent::SubAgentSessionSnapshot {
            key: child.clone(),
            transcript,
            status: SubAgentStatus::Completed,
        })
        .await
        .unwrap();
    open_focused_root_subagent(&mut harness).await;
    harness.render();

    press_key(&mut harness, KeyCode::Up).await;
    harness.render();
    let state = &harness.tui().app.monitored_sessions[&child];
    assert_eq!(state.transcript_focus, Some(11));
    assert!(
        state.scroll.position > 0,
        "focused last row must be visible"
    );

    for _ in 0..3 {
        press_key(&mut harness, KeyCode::Down).await;
    }
    assert_eq!(
        harness.tui().app.monitored_sessions[&child].transcript_focus,
        Some(11),
        "Down at the final row must remain bounded"
    );

    for _ in 0..15 {
        press_key(&mut harness, KeyCode::Up).await;
    }
    harness.render();
    let state = &harness.tui().app.monitored_sessions[&child];
    assert_eq!(state.transcript_focus, Some(0));
    assert_eq!(
        state.scroll.position, 0,
        "focused first row must be visible"
    );
}

#[tokio::test]
async fn root_session_change_aborts_child_monitors_and_discards_child_views() {
    let config = test_config_with_mock_client_and_agent("root", Some("root-one"));
    let mut tui = crate::types::Tui::init(&config).await.unwrap();
    tui.sync_session_activity_monitor();
    let child = monitored_key("worker", "child-session");
    tui.handle_tui_event(TuiEvent::Agent(AgentEvent::Turn(
        TurnEvent::SubAgentStarted {
            agent: child.agent.clone(),
            session_id: child.session_id.clone(),
        },
    )))
    .await
    .unwrap();
    tui.app.subagent_view_stack.push(child.clone());
    let abort_handle = tui.subagent_monitor_handles[&child].abort_handle();

    {
        let mut cfg = config.write();
        let replacement = harnx_runtime::config::session::new(&cfg, "root-two", None).unwrap();
        cfg.session = Some(replacement);
    }
    tui.app.transcript.clear();
    tui.sync_session_activity_monitor();

    tokio::time::timeout(Duration::from_secs(1), async {
        while !abort_handle.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("old child monitor should be aborted");
    assert!(tui.subagent_monitor_handles.is_empty());
    assert!(tui.app.monitored_sessions.is_empty());
    assert!(tui.app.subagent_view_stack.is_empty());
    assert_eq!(
        tui.subagent_monitor_root,
        Some((
            "root-two".to_string(),
            harnx_runtime::config::LOCAL_CLUSTER_KEY.to_string()
        ))
    );
}

#[tokio::test]
async fn child_live_events_do_not_mutate_parent_busy_or_streaming_state() {
    let mut tui = crate::types::Tui::init(&test_config()).await.unwrap();
    let child = monitored_key("worker", "isolated-child");
    tui.app.llm_busy = false;
    tui.app.streaming_open = false;
    tui.handle_tui_event(TuiEvent::SubAgentSessionEvent {
        key: child.clone(),
        event: AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("child output".into())],
        }),
    })
    .await
    .unwrap();

    assert!(!tui.app.llm_busy);
    assert!(!tui.app.streaming_open);
    assert!(matches!(
        tui.app.monitored_sessions[&child].transcript.as_slice(),
        [TranscriptItem::AssistantText { text, .. }] if text == "child output"
    ));
}

#[tokio::test]
async fn child_live_thoughts_strip_model_tags_and_ansi_sequences() {
    let mut tui = crate::types::Tui::init(&test_config()).await.unwrap();
    let child = monitored_key("worker", "clean-thought-child");

    tui.handle_tui_event(TuiEvent::SubAgentSessionEvent {
        key: child.clone(),
        event: AgentEvent::Model(ModelEvent::ThoughtChunk {
            blocks: vec![ContentBlock::Text(
                "<think>\u{1b}[31mreasoning\u{1b}[0m</think>".into(),
            )],
        }),
    })
    .await
    .unwrap();

    assert!(matches!(
        tui.app.monitored_sessions[&child].transcript.as_slice(),
        [TranscriptItem::ThoughtText(text)] if text == "reasoning"
    ));
}

#[tokio::test]
async fn child_final_without_streamed_chunks_preserves_prior_assistant_message() {
    let mut tui = crate::types::Tui::init(&test_config()).await.unwrap();
    let child = monitored_key("worker", "final-only-child");
    tui.app.monitored_sessions.insert(
        child.clone(),
        crate::types::MonitoredSessionState::new(SubAgentStatus::Running),
    );
    tui.app
        .monitored_sessions
        .get_mut(&child)
        .unwrap()
        .transcript = vec![assistant_text("prior turn")];

    tui.handle_tui_event(TuiEvent::SubAgentSessionEvent {
        key: child.clone(),
        event: AgentEvent::Model(ModelEvent::Final {
            output: "new final".into(),
            usage: Default::default(),
        }),
    })
    .await
    .unwrap();

    assert!(matches!(
        tui.app.monitored_sessions[&child].transcript.as_slice(),
        [TranscriptItem::AssistantText { text: prior, .. }, TranscriptItem::AssistantText { text: final_text, .. }]
            if prior == "prior turn" && final_text == "new final"
    ));
}

#[tokio::test]
async fn paste_is_ignored_while_child_transcript_is_fullscreen() {
    let mut tui = crate::types::Tui::init(&test_config()).await.unwrap();
    tui.set_input_text("unchanged draft");
    tui.app
        .subagent_view_stack
        .push(monitored_key("worker", "visible-child"));

    tui.handle_paste("hidden paste".into()).await;

    assert_eq!(tui.app.input.lines(), &["unchanged draft"]);
}

#[tokio::test]
async fn tui_switches_only_after_committed_handoff_and_ignores_late_source_completion() {
    let temp = tempfile::tempdir().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    create_agent_stubs(&temp.path().join("agents"), &["target"]);

    let config = test_config_with_mock_client_and_agent("source", Some("source-session"));
    let mut tui = crate::types::Tui::init(&config).await.unwrap();
    let source_task = harnx_runtime::utils::create_abort_signal();
    tui.current_prompt_abort = Some(source_task.clone());
    tui.app.llm_busy = true;
    tui.app.streaming_open = true;

    tui.handle_tui_event(TuiEvent::Agent(AgentEvent::Turn(
        TurnEvent::HandoffRequested {
            agent: "target".into(),
            session_id: Some("target-session".into()),
        },
    )))
    .await
    .unwrap();
    {
        let cfg = config.read();
        assert_eq!(cfg.agent.as_ref().map(|agent| agent.name()), Some("source"));
        assert_eq!(
            cfg.session.as_ref().map(|session| session.id()),
            Some("source-session")
        );
    }

    tui.handle_tui_event(TuiEvent::Agent(AgentEvent::Session(
        harnx_core::event::SessionEvent::HandoffCommitted {
            agent: "target".into(),
            session_id: "target-session".into(),
        },
    )))
    .await
    .unwrap();
    {
        let cfg = config.read();
        assert_eq!(cfg.agent.as_ref().map(|agent| agent.name()), Some("target"));
        assert_eq!(
            cfg.session.as_ref().map(|session| session.id()),
            Some("target-session")
        );
    }
    assert_eq!(
        tui.active_remote_session,
        Some((
            "target-session".into(),
            harnx_runtime::config::LOCAL_CLUSTER_KEY.into()
        ))
    );
    assert!(tui.current_prompt_abort.is_none());
    assert!(tui.app.llm_busy);
    assert!(!tui.app.streaming_open);

    tui.handle_tui_event(TuiEvent::PromptTaskFinished {
        task: source_task,
        error: Some("late source failure".into()),
    })
    .await
    .unwrap();
    assert!(
        tui.app.llm_busy,
        "late source completion must not mark the selected target idle"
    );
    assert!(tui.app.transcript.iter().all(
        |item| !matches!(item, TranscriptItem::ErrorText(error) if error.contains("late source"))
    ));
}
