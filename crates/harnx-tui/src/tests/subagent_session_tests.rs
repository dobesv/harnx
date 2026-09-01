use super::{
    create_agent_stubs, normalize_screen, test_config, test_config_with_mock_client_and_agent,
};
use crate::test_utils::{TestEnvironment, TuiTestHarness, ENV_LOCK};
use crate::types::{MonitoredSessionKey, SubAgentStatus, SubAgentView, TranscriptItem, TuiEvent};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::event::{
    AgentEvent, ContentBlock, ModelEvent, SubAgentProgress, SubAgentProgressStatus, ToolEvent,
    TurnEvent,
};
use std::time::Duration;

fn monitored_key(agent: &str, session_id: &str) -> MonitoredSessionKey {
    MonitoredSessionKey {
        cluster: harnx_runtime::config::LOCAL_CLUSTER_KEY.to_string(),
        agent: agent.to_string(),
        session_id: session_id.to_string(),
    }
}

fn subagent_view(key: MonitoredSessionKey) -> SubAgentView {
    SubAgentView {
        key,
        status: SubAgentStatus::Running,
        progress: None,
    }
}

fn open_subagent_keys(tui: &crate::types::Tui) -> Vec<MonitoredSessionKey> {
    tui.app
        .subagent_view_stack
        .iter()
        .map(|view| view.key.clone())
        .collect()
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
    emit_subagent_invocation_started(tui, key, Some("inv-1")).await;
}

async fn emit_subagent_invocation_started(
    tui: &mut crate::types::Tui,
    key: &MonitoredSessionKey,
    invocation_id: Option<&str>,
) {
    tui.handle_tui_event(TuiEvent::Agent(AgentEvent::Turn(
        TurnEvent::SubAgentStarted {
            agent: key.agent.clone(),
            session_id: key.session_id.clone(),
            invocation_id: invocation_id.map(str::to_string),
        },
    )))
    .await
    .unwrap();
}

fn subagent_progress(
    key: &MonitoredSessionKey,
    invocation_id: &str,
    status: SubAgentProgressStatus,
    elapsed_ms: u64,
) -> SubAgentProgress {
    SubAgentProgress {
        invocation_id: invocation_id.into(),
        agent: key.agent.clone(),
        session_id: key.session_id.clone(),
        status,
        elapsed_ms,
        usage: CompletionTokenUsage::new(Some(1_200), Some(345), Some(67)),
        tool_call_count: 4,
    }
}

fn completed_subagent_event(key: &MonitoredSessionKey) -> AgentEvent {
    let progress = subagent_progress(key, "inv-1", SubAgentProgressStatus::Done, 12_345);
    AgentEvent::Tool(ToolEvent::Completed {
        id: "delegate-call".into(),
        output: serde_json::json!({
            "response": "child response",
            "content": [{"type": "text", "text": "child response"}],
            "sub_agent": {
                "agent": key.agent,
                "session_id": key.session_id,
            },
            "sub_agent_progress": serde_json::to_value(&progress).unwrap()
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
        [TranscriptItem::SubAgentSession {
            key: row_key,
            status: SubAgentStatus::Running,
            ..
        }]
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

    let transcript = harness.tui().app.transcript.as_slice();
    assert!(
        matches!(
            transcript,
            [
                TranscriptItem::ToolResultMarkdown { .. },
                TranscriptItem::SubAgentSession {
                    key: row_key,
                    status: SubAgentStatus::Completed,
                    ..
                }
            ]
                if row_key == &key
        ),
        "transcript was: {:?}",
        transcript
    );
}

#[tokio::test]
async fn progress_animates_counts_elapsed_and_freezes_terminal_metrics() {
    let mut harness = TuiTestHarness::with_size(90, 12).await;
    harness.tui().clear_transcript();
    harness.tui().app.llm_busy = false;
    let key = monitored_key("researcher", "progress-session");
    emit_subagent_invocation_started(harness.tui(), &key, Some("inv-progress")).await;
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(AgentEvent::Turn(
            TurnEvent::SubAgentProgress(subagent_progress(
                &key,
                "inv-progress",
                SubAgentProgressStatus::Running,
                10_000,
            )),
        )))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(20)).await;
    let running_elapsed = match &harness.tui().app.transcript[0] {
        TranscriptItem::SubAgentSession {
            progress: Some(progress),
            ..
        } => progress.elapsed_ms(),
        other => panic!("expected progress row, got {other:?}"),
    };
    assert!(running_elapsed >= 10_020);
    assert!(
        !harness.tui().app.llm_busy,
        "child progress must not change root busy state"
    );

    harness.tui().app.spinner_index = 0;
    harness.render();
    let first_frame = harness.screen_contents();
    assert!(first_frame.contains('⠋'));
    assert!(first_frame.contains("in 1200  out 345  cache 67  tools 4"));
    harness.tui().app.spinner_index = 1;
    harness.render();
    assert!(harness.screen_contents().contains('⠙'));

    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(AgentEvent::Turn(
            TurnEvent::SubAgentProgress(subagent_progress(
                &key,
                "inv-progress",
                SubAgentProgressStatus::Done,
                12_345,
            )),
        )))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    match &harness.tui().app.transcript[0] {
        TranscriptItem::SubAgentSession {
            status: SubAgentStatus::Completed,
            progress: Some(progress),
            ..
        } => assert_eq!(progress.elapsed_ms(), 12_345),
        other => panic!("expected completed progress row, got {other:?}"),
    }
}

#[tokio::test]
async fn terminal_progress_is_not_reopened_by_child_monitor_or_late_progress() {
    let mut harness = TuiTestHarness::new().await;
    harness.tui().clear_transcript();
    harness.tui().app.llm_busy = true;
    let key = monitored_key("researcher", "terminal-session");
    emit_subagent_invocation_started(harness.tui(), &key, Some("inv-terminal")).await;
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(AgentEvent::Turn(
            TurnEvent::SubAgentProgress(subagent_progress(
                &key,
                "inv-terminal",
                SubAgentProgressStatus::Done,
                12_345,
            )),
        )))
        .await
        .unwrap();

    harness
        .tui()
        .handle_tui_event(TuiEvent::SubAgentSessionSnapshot {
            key: key.clone(),
            transcript: vec![],
            status: SubAgentStatus::Running,
        })
        .await
        .unwrap();
    harness
        .tui()
        .handle_tui_event(TuiEvent::SubAgentSessionEvent {
            key: key.clone(),
            event: AgentEvent::Turn(TurnEvent::Started),
        })
        .await
        .unwrap();
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(AgentEvent::Turn(
            TurnEvent::SubAgentProgress(subagent_progress(
                &key,
                "inv-terminal",
                SubAgentProgressStatus::Running,
                20_000,
            )),
        )))
        .await
        .unwrap();

    match &harness.tui().app.transcript[0] {
        TranscriptItem::SubAgentSession {
            status: SubAgentStatus::Completed,
            progress: Some(progress),
            ..
        } => {
            assert_eq!(progress.snapshot.status, SubAgentProgressStatus::Done);
            assert_eq!(progress.elapsed_ms(), 12_345);
        }
        other => panic!("expected terminal progress row, got {other:?}"),
    }
    assert!(
        harness.tui().app.llm_busy,
        "terminal child progress must remain done while the parent stays busy"
    );
}

#[tokio::test]
async fn invocation_ids_keep_reused_child_sessions_as_distinct_rows() {
    let mut harness = TuiTestHarness::new().await;
    harness.tui().clear_transcript();
    let key = monitored_key("researcher", "reused-session");

    for (invocation_id, elapsed_ms) in [("inv-1", 1_000), ("inv-2", 2_000)] {
        emit_subagent_invocation_started(harness.tui(), &key, Some(invocation_id)).await;
        harness
            .tui()
            .handle_tui_event(TuiEvent::Agent(AgentEvent::Turn(
                TurnEvent::SubAgentProgress(subagent_progress(
                    &key,
                    invocation_id,
                    SubAgentProgressStatus::Done,
                    elapsed_ms,
                )),
            )))
            .await
            .unwrap();
    }

    let invocations = harness
        .tui()
        .app
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::SubAgentSession { invocation_id, .. } => invocation_id.as_deref(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(invocations, ["inv-1", "inv-2"]);

    harness.tui().app.transcript_focus = Some(0);
    assert!(harness.tui().open_focused_root_subagent());
    let opened = harness.tui().app.subagent_view_stack.last().unwrap();
    let progress = opened.progress.as_ref().unwrap();
    assert_eq!(progress.snapshot.invocation_id, "inv-1");
    assert_eq!(progress.elapsed_ms(), 1_000);
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
    emit_subagent_invocation_started(harness.tui(), &key, Some("inv-2")).await;

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
    emit_subagent_invocation_started(harness.tui(), &parent, Some("inv-parent")).await;
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent(AgentEvent::Turn(
            TurnEvent::SubAgentProgress(subagent_progress(
                &parent,
                "inv-parent",
                SubAgentProgressStatus::Running,
                2_000,
            )),
        )))
        .await
        .unwrap();
    harness
        .tui()
        .handle_tui_event(TuiEvent::SubAgentSessionSnapshot {
            key: parent.clone(),
            transcript: vec![
                assistant_text("parent transcript"),
                TranscriptItem::SubAgentSession {
                    key: nested.clone(),
                    status: SubAgentStatus::Running,
                    invocation_id: None,
                    progress: None,
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
    assert_eq!(open_subagent_keys(harness.tui()), vec![parent]);
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
    assert_eq!(open_subagent_keys(harness.tui()), vec![parent.clone()]);
    press_key(&mut harness, KeyCode::Enter).await;
    assert_eq!(
        open_subagent_keys(harness.tui()),
        vec![parent.clone(), nested.clone()]
    );
    press_key(&mut harness, KeyCode::PageUp).await;
    assert!(!harness.tui().app.monitored_sessions[&nested].scroll.follow);
    press_key(&mut harness, KeyCode::Esc).await;
    assert_eq!(open_subagent_keys(harness.tui()), vec![parent]);
    press_key(&mut harness, KeyCode::Esc).await;
    assert!(harness.tui().app.subagent_view_stack.is_empty());
}

#[tokio::test]
async fn nested_progress_is_attached_to_the_parent_child_transcript() {
    let mut harness = TuiTestHarness::new().await;
    harness.tui().clear_transcript();
    let parent = monitored_key("researcher", "parent-session");
    let nested = monitored_key("reviewer", "nested-session");
    emit_subagent_invocation_started(harness.tui(), &parent, Some("parent-inv")).await;

    harness
        .tui()
        .handle_tui_event(TuiEvent::SubAgentSessionEvent {
            key: parent.clone(),
            event: AgentEvent::sub_agent(
                harnx_core::event::AgentSource {
                    agent: nested.agent.clone(),
                    session_id: Some(nested.session_id.clone()),
                    model: None,
                },
                AgentEvent::Turn(TurnEvent::SubAgentProgress(subagent_progress(
                    &nested,
                    "nested-inv",
                    SubAgentProgressStatus::Running,
                    3_000,
                ))),
            ),
        })
        .await
        .unwrap();

    assert!(matches!(
        harness.tui().app.monitored_sessions[&parent].transcript.as_slice(),
        [TranscriptItem::SubAgentSession {
            key,
            invocation_id: Some(invocation_id),
            progress: Some(_),
            ..
        }] if key == &nested && invocation_id == "nested-inv"
    ));
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
    assert_eq!(open_subagent_keys(harness.tui()), vec![parent]);
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
            invocation_id: None,
        },
    )))
    .await
    .unwrap();
    tui.app
        .subagent_view_stack
        .push(subagent_view(child.clone()));
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
        .push(subagent_view(monitored_key("worker", "visible-child")));

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

#[tokio::test]
async fn live_subagent_reply_appears_before_status_row() {
    let mut harness = TuiTestHarness::with_size(60, 12).await;
    let tui = harness.tui();
    tui.clear_transcript();
    let key = monitored_key("agentA", "sessionX");

    // 1) Tool started (prompt)
    tui.handle_tui_event(TuiEvent::Agent(AgentEvent::Tool(ToolEvent::Started {
        id: "delegate-call".into(),
        name: "agentA_session_prompt".to_string(),
        kind: harnx_core::event::ToolKind::Other,
        markdown: None,
        input: serde_json::json!({"message": "do thing"}),
        locations: vec![],
    })))
    .await
    .unwrap();

    // Verify ToolCall is in transcript
    assert_eq!(tui.app.transcript.len(), 1);
    assert!(matches!(
        tui.app.transcript[0],
        TranscriptItem::ToolCall { .. }
    ));

    // 2) SubAgentStarted (status row) - use invocation-based start
    emit_subagent_invocation_started(tui, &key, Some("inv-1")).await;
    assert_eq!(tui.app.transcript.len(), 2);
    assert!(matches!(
        tui.app.transcript[1],
        TranscriptItem::SubAgentSession { .. }
    ));

    // For production fidelity, emit a standalone terminal TurnEvent::SubAgentProgress BEFORE the ToolEvent::Completed
    tui.handle_tui_event(TuiEvent::Agent(AgentEvent::Turn(
        TurnEvent::SubAgentProgress(subagent_progress(
            &key,
            "inv-1",
            SubAgentProgressStatus::Done,
            12_345,
        )),
    )))
    .await
    .unwrap();

    // 3) Tool completed (reply)
    tui.handle_tui_event(TuiEvent::Agent(completed_subagent_event(&key)))
        .await
        .unwrap();

    // Verify order: ToolCall, ToolResultMarkdown, SubAgentSession
    assert_eq!(tui.app.transcript.len(), 3);
    assert!(matches!(
        tui.app.transcript[0],
        TranscriptItem::ToolCall { .. }
    ));
    assert!(matches!(
        &tui.app.transcript[1],
        TranscriptItem::ToolResultMarkdown { text, .. } if text == "child response"
    ));
    assert!(matches!(
        &tui.app.transcript[2],
        TranscriptItem::SubAgentSession {
            status: SubAgentStatus::Completed,
            ..
        }
    ));
}

#[tokio::test]
async fn nested_subagent_reply_appears_in_parent_child_transcript() {
    let mut harness = TuiTestHarness::new().await;
    harness.tui().clear_transcript();
    let parent = monitored_key("researcher", "parent-session");
    let nested = monitored_key("reviewer", "nested-session");

    // Parent sub-agent session started.
    emit_subagent_invocation_started(harness.tui(), &parent, Some("parent-inv")).await;

    // Send a sub-agent started (Progress) event for the nested session through the parent's TuiEvent wrapper.
    harness
        .tui()
        .handle_tui_event(TuiEvent::SubAgentSessionEvent {
            key: parent.clone(),
            event: AgentEvent::sub_agent(
                harnx_core::event::AgentSource {
                    agent: nested.agent.clone(),
                    session_id: Some(nested.session_id.clone()),
                    model: None,
                },
                AgentEvent::Turn(TurnEvent::SubAgentProgress(subagent_progress(
                    &nested,
                    "inv-1", // Match completed_subagent_event's id
                    SubAgentProgressStatus::Running,
                    3_000,
                ))),
            ),
        })
        .await
        .unwrap();

    // Now send the completion tool event nested inside the parent's SubAgentSessionEvent.
    harness
        .tui()
        .handle_tui_event(TuiEvent::SubAgentSessionEvent {
            key: parent.clone(),
            event: AgentEvent::sub_agent(
                harnx_core::event::AgentSource {
                    agent: nested.agent.clone(),
                    session_id: Some(nested.session_id.clone()),
                    model: None,
                },
                completed_subagent_event(&nested),
            ),
        })
        .await
        .unwrap();

    // Main transcript should only contain the parent's subagent session row
    assert_eq!(harness.tui().app.transcript.len(), 1);

    let parent_transcript = &harness.tui().app.monitored_sessions[&parent].transcript;
    // Parent's child transcript should contain the nested result and the nested subagent session
    assert_eq!(parent_transcript.len(), 2);

    assert!(matches!(
        &parent_transcript[0],
        TranscriptItem::ToolResultMarkdown { text, .. } if text == "child response"
    ));
    assert!(matches!(
        &parent_transcript[1],
        TranscriptItem::SubAgentSession {
            key,
            status: SubAgentStatus::Completed,
            ..
        } if key == &nested
    ));
}

#[tokio::test]
async fn live_subagent_empty_response_omits_reply_row() {
    let mut harness = TuiTestHarness::with_size(60, 12).await;
    let tui = harness.tui();
    tui.clear_transcript();
    let key = monitored_key("agentA", "sessionX");

    // 1) Tool started (prompt)
    tui.handle_tui_event(TuiEvent::Agent(AgentEvent::Tool(ToolEvent::Started {
        id: "delegate-call".into(),
        name: "agentA_session_prompt".to_string(),
        kind: harnx_core::event::ToolKind::Other,
        markdown: None,
        input: serde_json::json!({"message": "do thing"}),
        locations: vec![],
    })))
    .await
    .unwrap();

    // 2) SubAgentStarted (status row) - use invocation-based start
    emit_subagent_invocation_started(tui, &key, Some("inv-1")).await;

    // 3) Tool completed (empty reply)
    let progress = subagent_progress(&key, "inv-1", SubAgentProgressStatus::Done, 12_345);
    let empty_response_event = AgentEvent::Tool(ToolEvent::Completed {
        id: "delegate-call".into(),
        output: serde_json::json!({
            "response": "",
            "content": [{"type": "text", "text": ""}],
            "sub_agent": {
                "agent": key.agent.clone(),
                "session_id": key.session_id.clone(),
            },
            "sub_agent_progress": serde_json::to_value(&progress).unwrap()
        }),
        markdown: Some("".into()),
    });

    tui.handle_tui_event(TuiEvent::Agent(empty_response_event))
        .await
        .unwrap();

    // Verify order: ToolCall, SubAgentSession (No ToolResultMarkdown)
    assert_eq!(tui.app.transcript.len(), 2);
    assert!(matches!(
        tui.app.transcript[0],
        TranscriptItem::ToolCall { .. }
    ));
    assert!(matches!(
        &tui.app.transcript[1],
        TranscriptItem::SubAgentSession {
            status: SubAgentStatus::Completed,
            ..
        }
    ));
}
