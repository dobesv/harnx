use super::*;
use crate::types::{ConfirmView, ModalState};
use harnx_core::event::{StatusLine, TurnEvent};

#[tokio::test]
async fn bookkeeping_and_ignored_events_keep_projection_order() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.clear_transcript();
    tui.render_agent_event(AgentEvent::User(UserEvent::Message {
        content: "[Replay](cid:replay)".into(),
    }))
    .await;
    tui.app.pending_thought_text = "pending thought".into();
    let source = AgentSource {
        agent: "child".into(),
        ..Default::default()
    };
    tui.render_agent_event(AgentEvent::sub_agent(
        source,
        AgentEvent::Session(SessionEvent::LogSeqAssigned { seq: 17 }),
    ))
    .await;
    assert_eq!(tui.app.pending_tool_seq, Some(17));
    assert_eq!(tui.app.pending_thought_text, "pending thought");
    assert_eq!(
        tui.app.transcript.len(),
        2,
        "sequence assignment must not create a heading or flush thoughts"
    );
    assert!(matches!(
        &tui.app.transcript[0],
        TranscriptItem::UserText {
            seq: None,
            timestamp: None,
            ..
        }
    ));
    tui.render_agent_event(AgentEvent::Tool(ToolEvent::Progress {
        id: "old".into(),
        text: "legacy progress".into(),
    }))
    .await;
    assert!(
        matches!(tui.app.transcript.last(), Some(TranscriptItem::ThoughtText(text)) if text == "pending thought")
    );
}

#[tokio::test]
async fn ignored_event_families_do_not_add_output_rows() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.clear_transcript();
    let length = tui.app.transcript.len();
    for event in [
        AgentEvent::Status(StatusLine {
            text: "status".into(),
        }),
        AgentEvent::Session(SessionEvent::Saved {
            path: "session".into(),
        }),
        AgentEvent::Turn(TurnEvent::RetryAttempt {
            attempt: 2,
            reason: "retry".into(),
        }),
        AgentEvent::Model(ModelEvent::Usage {
            input: 1,
            output: 2,
            cached: 0,
            cache_write: 0,
            session_label: None,
        }),
    ] {
        tui.render_agent_event(event).await;
        assert_eq!(
            tui.app.transcript.len(),
            length,
            "ignored event must not create an output row"
        );
    }
}

#[tokio::test]
async fn projected_notices_sessions_and_model_chunks_keep_exact_text() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.clear_transcript();
    for (event, expected) in [
        (
            AgentEvent::Notice(NoticeEvent::Info("\x1b[31minfo\x1b[0m\n".into())),
            "info",
        ),
        (
            AgentEvent::Notice(NoticeEvent::Warning("warning\n".into())),
            "⚠ warning",
        ),
        (
            AgentEvent::Notice(NoticeEvent::Error("error\n".into())),
            "error: error",
        ),
        (
            AgentEvent::Session(SessionEvent::Generic {
                text: "generic\n".into(),
            }),
            "generic\n",
        ),
    ] {
        tui.render_agent_event(event).await;
        assert!(
            matches!(tui.app.transcript.last(), Some(TranscriptItem::SystemText(text)) if text == expected)
        );
    }
    tui.render_agent_event(AgentEvent::Notice(NoticeEvent::Info("\n".into())))
        .await;
    assert_eq!(tui.app.transcript.len(), 4);
    for text in ["<think>part", "\n", "two</think>"] {
        tui.render_agent_event(AgentEvent::Model(ModelEvent::ThoughtChunk {
            blocks: vec![ContentBlock::Text(text.into())],
        }))
        .await;
    }
    assert_eq!(tui.app.pending_thought_text, "part\ntwo");
    tui.render_agent_event(AgentEvent::Model(ModelEvent::MessageChunk {
        blocks: vec![ContentBlock::Text("answer".into())],
    }))
    .await;
    assert!(
        matches!(&tui.app.transcript[4], TranscriptItem::ThoughtText(text) if text == "part\ntwo")
    );
    assert!(
        matches!(&tui.app.transcript[5], TranscriptItem::AssistantText { text, .. } if text == "answer")
    );
}

#[tokio::test]
async fn attachment_failures_and_named_detach_consume_only_command_line() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.clear_transcript();
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing.txt");
    tui.set_input_text(&format!("draft\n.attach {}", missing.display()));
    assert!(tui.try_handle_attach_command().await);
    assert_eq!(tui.app.input.lines().join("\n"), "draft");
    assert!(
        matches!(tui.app.transcript.last(), Some(TranscriptItem::ErrorText(text)) if text == &format!("File not found: {}", missing.display()))
    );
    let source = directory.path().join("file.txt");
    std::fs::write(&source, "contents").unwrap();
    for _ in 0..2 {
        tui.set_input_text(&format!("draft\n.attach {}", source.display()));
        assert!(tui.try_handle_attach_command().await);
        assert_eq!(tui.app.input.lines().join("\n"), "draft");
    }
    assert_eq!(tui.app.attachments[0].display_name, "file.txt");
    assert_eq!(tui.app.attachments[1].display_name, "file.txt (1)");
    let removed = tui.app.attachments[0].path.clone();
    std::fs::remove_file(&removed).unwrap();
    tui.set_input_text("draft\n.detach file.txt");
    assert!(tui.try_handle_attach_command().await);
    assert_eq!(
        tui.app.attachments.len(),
        1,
        "failed removal still detaches the named row"
    );
    assert!(
        matches!(tui.app.transcript.last(), Some(TranscriptItem::ErrorText(text)) if text.starts_with("Failed to remove detached attachment file file.txt:"))
    );
    tui.set_input_text("draft\n.detach");
    assert!(tui.try_handle_attach_command().await);
    assert!(tui.app.attachments.is_empty());
    assert!(tui.app.attachment_dir.is_none());
    assert_eq!(tui.app.input.lines().join("\n"), "draft");
}

#[tokio::test]
async fn reconciliation_preserves_exact_mutation_prefixes() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    for command in [
        ".empty session",
        ".reset session",
        ".reset repl",
        ".compact session",
        ".edit session",
        ".edit message 1",
        ".delete message 1",
        ".rewind 1",
    ] {
        tui.app.transcript = vec![TranscriptItem::SystemText("sentinel".into())];
        tui.reconcile_transcript_after_command(None, None, command)
            .await;
        assert!(
            tui.app.transcript.iter().all(
                |item| !matches!(item, TranscriptItem::SystemText(text) if text == "sentinel")
            ),
            "{command}"
        );
    }
    for command in [
        ".echo text",
        ".edit message",
        ".delete message",
        ".rewind",
        " .reset session",
    ] {
        tui.app.transcript = vec![TranscriptItem::SystemText("sentinel".into())];
        tui.reconcile_transcript_after_command(None, None, command)
            .await;
        assert!(
            matches!(tui.app.transcript.as_slice(), [TranscriptItem::SystemText(text)] if text == "sentinel"),
            "{command}"
        );
    }
}

#[tokio::test]
async fn confirmation_edits_and_submitting_guard_share_activity_timestamp() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.app.modal = Some(tool_confirmation_modal(
        serde_json::json!({}),
        ConfirmView::Template,
        false,
        None,
        None,
    ));
    let initial = std::time::Instant::now() - Duration::from_secs(3);
    if let Some(ModalState::ConfirmToolUse(state)) = tui.app.modal.as_mut() {
        state.last_key_at = initial;
    }
    tui.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert_eq!(confirmation_state(&tui).view, ConfirmView::Template);
    assert!(confirmation_state(&tui).last_key_at > initial);
    for key in [
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
    ] {
        tui.handle_key(key).await.unwrap();
    }
    assert_eq!(confirmation_state(&tui).message.lines().len(), 4);
}

#[tokio::test]
async fn confirmation_submitting_keys_refresh_activity_without_resolving() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    let mut modal = tool_confirmation_modal(
        serde_json::json!({}),
        ConfirmView::Template,
        false,
        None,
        None,
    );
    let initial = std::time::Instant::now() - Duration::from_secs(3);
    if let ModalState::ConfirmToolUse(state) = &mut modal {
        state.submitting = true;
        state.last_key_at = initial;
    }
    tui.app.modal = Some(modal);
    for key in [
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
    ] {
        tui.handle_key(key).await.unwrap();
        assert!(confirmation_state(&tui).submitting);
        assert!(confirmation_state(&tui).last_key_at > initial);
    }
}

#[tokio::test]
async fn picker_boundaries_and_invalid_selection_keep_modal_and_draft() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.set_input_text("draft");
    tui.app.modal = Some(ModalState::AgentPicker {
        agents: vec!["alpha".into(), "beta".into()],
        selected: 0,
        query: "missing".into(),
    });
    for key in [KeyCode::Up, KeyCode::Down, KeyCode::Enter] {
        tui.handle_key(KeyEvent::new(key, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(
            matches!(tui.app.modal.as_ref(), Some(ModalState::AgentPicker { selected: 0, query, .. }) if query == "missing")
        );
    }
    tui.app.modal = Some(ModalState::SessionPicker {
        sessions: vec![],
        selected: 1,
        origin_agent: Some("origin".into()),
        origin_session: Some("session".into()),
        error: Some("old error".into()),
    });
    tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(
        matches!(tui.app.modal.as_ref(), Some(ModalState::SessionPicker { selected: 1, origin_agent: Some(agent), origin_session: Some(session), error: None, .. }) if agent == "origin" && session == "session")
    );
    assert_eq!(tui.app.input.lines().join("\n"), "draft");
}

fn confirmation_state(tui: &Tui) -> &crate::types::ConfirmToolUseState {
    match tui.app.modal.as_ref() {
        Some(ModalState::ConfirmToolUse(state)) => state,
        other => panic!("expected confirmation modal, got {other:?}"),
    }
}
