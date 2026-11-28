//! Regression tests for DI-PH1: destination commits and initial-error
//! replacements must clear foreign detached read views opened from retained
//! prior-session rows while restoring guarded tray focus when a confirmation
//! is pending, without breaking `#2953` same-target preservation.

use super::cross_session_transcript_action_tests::{
    active_session_id, open_session_picker_and_select, recv_history_event, send_key,
};
use super::*;
use crate::event_isolation::EventStamp;
use crate::interaction_confirmation::{InteractionEvent, InteractionKey, PendingConfirmation};
use crate::types::{
    InteractionFocus, ModalState, MonitoredSessionKey, SubAgentStatus, TranscriptItem, TuiEvent,
};
use crossterm::event::{KeyCode, KeyModifiers};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::{watch, Notify};

fn user_row(seq: usize, text: &str) -> TranscriptItem {
    TranscriptItem::UserText {
        text: text.to_string(),
        seq: Some(seq),
        timestamp: None,
    }
}

fn link_row(text: &str, url: &str) -> TranscriptItem {
    TranscriptItem::MarkdownLink {
        text: text.to_string(),
        url: url.to_string(),
    }
}

fn pending_confirmation_for(id: &str, message: &str) -> PendingConfirmation {
    PendingConfirmation {
        key: InteractionKey {
            id: id.to_string(),
            request_seq: 3,
        },
        kind: "confirm".to_string(),
        message: message.to_string(),
        payload: serde_json::json!({ "message": message }),
        pending_count: 1,
    }
}

fn install_cid_docs(tui: &mut Tui) {
    tui.cid_resolve_override = Some(Arc::new(|url: String| {
        Box::pin(async move {
            let text = match url.as_str() {
                "cid:plan:acme/sess-a/doc" => {
                    "# Session A Doc\nSee [nested A](cid:plan:acme/sess-a/nested).".to_string()
                }
                "cid:plan:acme/sess-a/nested" => "# Session A Nested\nPrivate to A.".to_string(),
                "cid:plan:acme/sess-b/doc" => "# Session B Doc\nOwned by B.".to_string(),
                other => anyhow::bail!("unexpected cid url {other}"),
            };
            Ok(harnx_blob_store::ResolvedBlob {
                mime_type: "text/markdown".into(),
                bytes: text.into_bytes(),
                etag: None,
                immutable: true,
            })
        })
    }));
}

fn install_gated_b_loader(
    tui: &mut Tui,
    fail_b_load: bool,
    b_rows: Vec<TranscriptItem>,
) -> (Arc<Notify>, watch::Sender<bool>) {
    let load_started = Arc::new(Notify::new());
    let (release_tx, release_rx) = watch::channel(false);
    let started_tx = load_started.clone();
    let gate_first_b = Arc::new(AtomicBool::new(true));
    tui.history_load_override = Some(Arc::new(move |cfg, _mode, abort| {
        let started_tx = started_tx.clone();
        let mut release_rx = release_rx.clone();
        let gate_first_b = gate_first_b.clone();
        let b_rows = b_rows.clone();
        Box::pin(async move {
            let sid = cfg
                .read()
                .session
                .as_ref()
                .map(|s| s.id().to_string())
                .unwrap_or_default();
            if sid == "session-b" && gate_first_b.swap(false, Ordering::SeqCst) {
                started_tx.notify_one();
                tokio::select! {
                    _ = harnx_core::abort::wait_abort_signal(&abort) => {
                        anyhow::bail!("history load aborted")
                    }
                    _ = release_rx.wait_for(|released| *released) => {}
                }
                if fail_b_load {
                    anyhow::bail!("session-b history temporarily unavailable");
                }
            }
            Ok(b_rows)
        })
    }));
    (load_started, release_tx)
}

fn draw_test_frame(tui: &mut Tui) {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
    tui.draw_with_session_activity(&mut terminal).unwrap();
}

async fn deliver_b_confirmation(tui: &mut Tui, a_storage_key: &str, id: &str, message: &str) {
    assert_eq!(active_session_id(tui).as_deref(), Some("session-b"));
    draw_test_frame(tui);
    let expected_storage_key = tui
        .session_activity_destination()
        .map(|(sid, _)| sid)
        .expect("session-b activity destination");
    assert_ne!(expected_storage_key, a_storage_key);
    let (target_sid, generation) = tui.current_interaction_generation_for_test();
    assert_eq!(target_sid, Some(expected_storage_key.as_str()));
    let pending = pending_confirmation_for(id, message);
    tui.handle_tui_event(TuiEvent::Interaction(InteractionEvent::Snapshot {
        generation,
        pending: Ok(Some(pending)),
    }))
    .await
    .unwrap();
}

async fn release_and_apply_b_rebuild(tui: &mut Tui, release_load: &watch::Sender<bool>) {
    let b_seq = tui.history_rebuild_seq;
    let _ = release_load.send(true);
    let done = recv_history_event(
        tui,
        |ev| matches!(ev, TuiEvent::HistoryRebuildFinished(done) if done.seq == b_seq),
    )
    .await;
    tui.handle_tui_event(done).await.unwrap();
}

#[tokio::test]
async fn pick_b_reopen_a_doc_then_b_confirmation_clears_foreign_doc_on_b_commit_and_keeps_same_target_views(
) {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript = vec![
        user_row(1, "Session A secret prompt"),
        link_row("Session A plan", "cid:plan:acme/sess-a/doc"),
    ];
    install_cid_docs(&mut tui);
    let a_storage_key = tui.session_activity_destination().unwrap().0;
    let b_rows = vec![
        user_row(1, "Session B prompt"),
        link_row("Session B plan", "cid:plan:acme/sess-b/doc"),
    ];
    let (load_started, release_load) = install_gated_b_loader(&mut tui, false, b_rows);

    // Pick B while A rows remain displayed and B's load is gated.
    open_session_picker_and_select(&mut tui, "session-b").await;
    load_started.notified().await;
    assert_eq!(active_session_id(&tui).as_deref(), Some("session-b"));
    assert!(tui.app.doc_view.is_none());

    // Reopen A's root MarkdownLink document and drill into nested A doc.
    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    assert_eq!(
        tui.app.doc_view.as_ref().map(|d| d.url.as_str()),
        Some("cid:plan:acme/sess-a/doc")
    );
    send_key(&mut tui, KeyCode::Tab, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    assert_eq!(
        tui.app.doc_view.as_ref().map(|d| d.url.as_str()),
        Some("cid:plan:acme/sess-a/nested")
    );
    assert_eq!(tui.app.doc_history.len(), 1);
    assert!(tui.app.detail_view_open);

    // B's confirmation arrives while A's detached doc is open -> Reading focus.
    deliver_b_confirmation(&mut tui, &a_storage_key, "b-req-1", "Allow B command?").await;
    assert!(tui.interaction_pending());
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);

    // Release B's gated history commit: foreign A doc must vanish and guarded tray must return.
    release_and_apply_b_rebuild(&mut tui, &release_load).await;
    assert!(tui.app.doc_view.is_none(), "foreign A doc must be cleared");
    assert!(tui.app.doc_history.is_empty());
    assert!(!tui.app.detail_view_open);
    assert!(tui.app.detail_view_entry.is_none());
    assert!(tui.app.detail_view_text.is_none());
    assert!(tui.app.detail_view_title.is_none());
    assert!(tui.app.archive_browser.is_none());
    assert!(!tui.app.transcript_browsing);
    assert!(tui.app.transcript_focus.is_none());
    assert!(tui.app.transcript.iter().any(
        |item| matches!(item, TranscriptItem::UserText { text, .. } if text == "Session B prompt")
    ));
    assert!(!tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::UserText { text, .. } if text == "Session A secret prompt"
    )));

    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    let Some(ModalState::ConfirmInteraction(state)) = &tui.app.modal else {
        panic!("expected B confirmation modal");
    };
    assert_eq!(state.pending.key.id, "b-req-1");
    assert!(
        state.decision_guard_until.is_some(),
        "clearing the last foreign read view must arm the return-safety guard"
    );
    // Return-safety guard rejects an immediate Ctrl+D decision Press without
    // submitting or mutating the tray note.
    let note_before = state.note.lines().join("\n");
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::CONTROL).await;
    let Some(ModalState::ConfirmInteraction(state)) = &tui.app.modal else {
        panic!("return-safety guard must keep B confirmation modal open");
    };
    assert_eq!(state.pending.key.id, "b-req-1");
    assert!(!state.submitting);
    assert_eq!(state.note.lines().join("\n"), note_before);

    // #2953 same-target preservation: once on B, opening B's own document and
    // receiving a same-target background refresh keeps B's document and Reading focus.
    send_key(&mut tui, KeyCode::Esc, KeyModifiers::NONE).await;
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    assert_eq!(
        tui.app.doc_view.as_ref().map(|d| d.url.as_str()),
        Some("cid:plan:acme/sess-b/doc")
    );
    let stamp = EventStamp::live(&tui.live_events);
    tui.schedule_shared_session_refresh(&stamp);
    tui.flush_history_work_for_test().await;
    assert_eq!(
        tui.app.doc_view.as_ref().map(|d| d.url.as_str()),
        Some("cid:plan:acme/sess-b/doc"),
        "same-target refresh on B must preserve B's own document (#2953)"
    );
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);
}

#[tokio::test]
async fn pick_b_confirmation_before_a_doc_reopen_clears_foreign_views_and_arms_guard_on_b_initial_error(
) {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript = vec![
        user_row(1, "Session A secret prompt"),
        link_row("Session A plan", "cid:plan:acme/sess-a/doc"),
    ];
    install_cid_docs(&mut tui);
    let a_storage_key = tui.session_activity_destination().unwrap().0;
    let (load_started, release_load) = install_gated_b_loader(&mut tui, true, vec![]);

    open_session_picker_and_select(&mut tui, "session-b").await;
    load_started.notified().await;

    // B confirmation arrives first (plain tray, no guard yet).
    deliver_b_confirmation(
        &mut tui,
        &a_storage_key,
        "b-req-err",
        "Allow B before error?",
    )
    .await;
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    let Some(ModalState::ConfirmInteraction(state)) = &tui.app.modal else {
        panic!("expected B confirmation modal");
    };
    assert!(state.decision_guard_until.is_none());

    // User presses Esc from tray to browse retained A rows and opens A's document.
    send_key(&mut tui, KeyCode::Esc, KeyModifiers::NONE).await;
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    assert_eq!(
        tui.app.doc_view.as_ref().map(|d| d.url.as_str()),
        Some("cid:plan:acme/sess-a/doc")
    );
    assert!(tui.app.detail_view_open);

    // B's initial load fails and performs initial-error replacement.
    release_and_apply_b_rebuild(&mut tui, &release_load).await;
    assert!(tui.app.doc_view.is_none(), "foreign A doc must be cleared");
    assert!(tui.app.doc_history.is_empty());
    assert!(!tui.app.detail_view_open);
    assert!(tui.app.subagent_view_stack.is_empty());
    assert!(!tui.app.transcript_browsing);
    assert!(tui.app.transcript_focus.is_none());
    assert!(!tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::UserText { text, .. } if text == "Session A secret prompt"
    )));
    assert!(tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::ErrorText(text) if text.contains("session-b history temporarily unavailable")
    )));
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    let Some(ModalState::ConfirmInteraction(state)) = &tui.app.modal else {
        panic!("expected B confirmation modal");
    };
    assert!(state.decision_guard_until.is_some());
    let note_before = state.note.lines().join("\n");
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::CONTROL).await;
    let Some(ModalState::ConfirmInteraction(state)) = &tui.app.modal else {
        panic!("return-safety guard must keep B confirmation modal open");
    };
    assert_eq!(state.pending.key.id, "b-req-err");
    assert!(!state.submitting);
    assert_eq!(state.note.lines().join("\n"), note_before);
}

#[tokio::test]
async fn pick_b_reopen_a_child_after_monitor_root_moves_clears_child_stack_on_b_commit() {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    let a_child_key = MonitoredSessionKey {
        cluster: harnx_runtime::config::LOCAL_CLUSTER_KEY.to_string(),
        agent: "worker-a".to_string(),
        session_id: "session-a-child".to_string(),
    };
    tui.app.transcript = vec![
        user_row(1, "Session A secret prompt"),
        TranscriptItem::SubAgentSession {
            key: a_child_key.clone(),
            status: SubAgentStatus::Completed,
            invocation_id: Some("a-child-inv".to_string()),
            progress: None,
        },
    ];
    let a_storage_key = tui.session_activity_destination().unwrap().0;
    let (load_started, release_load) =
        install_gated_b_loader(&mut tui, false, vec![user_row(1, "Session B prompt")]);

    // Pick B; monitor root moves to B while A's rows remain retained and B's load is pending.
    open_session_picker_and_select(&mut tui, "session-b").await;
    load_started.notified().await;
    assert_eq!(active_session_id(&tui).as_deref(), Some("session-b"));
    assert_eq!(
        tui.subagent_monitor_root,
        tui.session_activity_destination(),
        "monitor root must already point at destination B while B load is pending"
    );
    assert!(tui.app.subagent_view_stack.is_empty());

    // Open retained A child via real keys after monitor root moved to B.
    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    assert_eq!(
        tui.app
            .subagent_view_stack
            .iter()
            .map(|view| &view.key)
            .collect::<Vec<_>>(),
        vec![&a_child_key]
    );

    // B confirmation arrives while the retained A child view is open -> Reading focus.
    deliver_b_confirmation(
        &mut tui,
        &a_storage_key,
        "b-req-child",
        "Allow B while A child open?",
    )
    .await;
    assert_eq!(
        tui.app
            .subagent_view_stack
            .iter()
            .map(|view| &view.key)
            .collect::<Vec<_>>(),
        vec![&a_child_key]
    );
    assert!(tui.interaction_pending());
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);
    let Some(ModalState::ConfirmInteraction(state)) = &tui.app.modal else {
        panic!("expected pending B confirmation modal while Reading A child view");
    };
    assert_eq!(state.pending.key.id, "b-req-child");

    // B's replacement commit clears the populated A child stack and returns the
    // same B confirmation to the guarded Tray.
    release_and_apply_b_rebuild(&mut tui, &release_load).await;
    assert!(
        tui.app.subagent_view_stack.is_empty(),
        "foreign A child view stack must be cleared on B replacement"
    );
    assert!(!tui.app.transcript_browsing);
    assert!(tui.app.transcript_focus.is_none());
    assert!(tui.app.transcript.iter().any(
        |item| matches!(item, TranscriptItem::UserText { text, .. } if text == "Session B prompt")
    ));
    assert!(!tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::UserText { text, .. } if text == "Session A secret prompt"
    )));
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    let Some(ModalState::ConfirmInteraction(state)) = &tui.app.modal else {
        panic!("expected same B confirmation modal");
    };
    assert_eq!(state.pending.key.id, "b-req-child");
    assert!(
        state.decision_guard_until.is_some(),
        "clearing the last foreign child view must arm the return-safety guard"
    );
    let note_before = state.note.lines().join("\n");
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::CONTROL).await;
    let Some(ModalState::ConfirmInteraction(state)) = &tui.app.modal else {
        panic!("return-safety guard must keep B confirmation modal open");
    };
    assert_eq!(state.pending.key.id, "b-req-child");
    assert!(!state.submitting);
    assert_eq!(state.note.lines().join("\n"), note_before);
}

#[tokio::test]
async fn pick_b_plain_tray_without_reading_view_keeps_tray_unarmed_on_b_commit() {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript = vec![user_row(1, "Session A prompt")];
    let a_storage_key = tui.session_activity_destination().unwrap().0;
    let (load_started, release_load) =
        install_gated_b_loader(&mut tui, false, vec![user_row(1, "Session B prompt")]);

    open_session_picker_and_select(&mut tui, "session-b").await;
    load_started.notified().await;

    deliver_b_confirmation(
        &mut tui,
        &a_storage_key,
        "b-req-tray",
        "Allow B plain tray?",
    )
    .await;
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);

    release_and_apply_b_rebuild(&mut tui, &release_load).await;
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    let Some(ModalState::ConfirmInteraction(state)) = &tui.app.modal else {
        panic!("expected B confirmation modal");
    };
    assert!(
        state.decision_guard_until.is_none(),
        "plain tray that never entered Reading must not gain a spurious return guard"
    );
}

#[tokio::test]
async fn target_switch_and_background_rebuild_reset_pending_attachment_preview() {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.handle_paste("pending preview body\n".repeat(40)).await;
    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    assert_eq!(tui.app.pending_attachment_view.focus, Some(0));
    assert!(tui.app.pending_attachment_view.confirming_delete);
    assert!(tui.app.detail_view_open);
    assert!(tui.app.detail_view_text.is_some());

    let (load_started, release_load) =
        install_gated_b_loader(&mut tui, false, vec![user_row(1, "Session B prompt")]);
    open_session_picker_and_select(&mut tui, "session-b").await;
    load_started.notified().await;

    assert!(tui.app.pending_attachment_view.focus.is_none());
    assert!(!tui.app.pending_attachment_view.confirming_delete);
    assert!(!tui.app.detail_view_open);
    assert!(tui.app.detail_view_text.is_none());
    assert!(tui.app.detail_view_title.is_none());
    assert_eq!(tui.app.attachments.len(), 1);

    release_and_apply_b_rebuild(&mut tui, &release_load).await;

    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    assert_eq!(tui.app.pending_attachment_view.focus, Some(0));
    assert!(tui.app.pending_attachment_view.confirming_delete);
    assert!(tui.app.detail_view_open);

    let stamp = EventStamp::live(&tui.live_events);
    tui.schedule_shared_session_refresh(&stamp);
    tui.flush_history_work_for_test().await;

    assert!(tui.app.pending_attachment_view.focus.is_none());
    assert!(!tui.app.pending_attachment_view.confirming_delete);
    assert!(!tui.app.detail_view_open);
    assert!(tui.app.detail_view_text.is_none());
    assert!(tui.app.detail_view_title.is_none());
    assert_eq!(tui.app.attachments.len(), 1);
    tui.cleanup_attachments();
}

#[tokio::test]
async fn pending_attachment_preview_respects_di_ph1_foreign_clear_and_same_target_interaction_preservation(
) {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.handle_paste("attachment preview text\n".repeat(40))
        .await;
    let a_storage_key = tui.session_activity_destination().unwrap().0;
    let (load_started, release_load) =
        install_gated_b_loader(&mut tui, false, vec![user_row(1, "Session B prompt")]);

    open_session_picker_and_select(&mut tui, "session-b").await;
    load_started.notified().await;

    // Open a pending attachment preview while B's initial load is still pending,
    // then deliver B's confirmation so Reading focus holds on that preview.
    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    assert_eq!(tui.app.pending_attachment_view.focus, Some(0));
    assert!(tui.app.detail_view_open);

    deliver_b_confirmation(
        &mut tui,
        &a_storage_key,
        "b-req-attach-foreign",
        "Allow B while attachment preview open?",
    )
    .await;
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);

    // B's destination commit drops the preview + focus and restores guarded Tray.
    release_and_apply_b_rebuild(&mut tui, &release_load).await;
    assert!(tui.app.pending_attachment_view.focus.is_none());
    assert!(!tui.app.pending_attachment_view.confirming_delete);
    assert!(!tui.app.detail_view_open);
    assert!(tui.app.detail_view_text.is_none());
    assert!(tui.app.detail_view_title.is_none());
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Tray);
    let Some(ModalState::ConfirmInteraction(state)) = &tui.app.modal else {
        panic!("expected guarded B confirmation modal");
    };
    assert!(state.decision_guard_until.is_some());

    // Clear the modal, open the attachment preview on B, receive a same-target
    // confirmation, and confirm #2953 background refresh keeps the preview.
    let (_, generation) = tui.current_interaction_generation_for_test();
    tui.handle_tui_event(TuiEvent::Interaction(InteractionEvent::Snapshot {
        generation,
        pending: Ok(None),
    }))
    .await
    .unwrap();
    assert!(tui.app.modal.is_none());

    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    assert_eq!(tui.app.pending_attachment_view.focus, Some(0));
    assert!(tui.app.detail_view_open);

    deliver_b_confirmation(
        &mut tui,
        &a_storage_key,
        "b-req-attach-same",
        "Allow same-target B?",
    )
    .await;
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);

    let stamp = EventStamp::live(&tui.live_events);
    tui.schedule_shared_session_refresh(&stamp);
    tui.flush_history_work_for_test().await;

    assert!(tui.app.detail_view_open);
    assert!(tui.app.detail_view_text.is_some());
    assert_eq!(tui.app.pending_attachment_view.focus, Some(0));
    assert_eq!(tui.app.interaction_focus, InteractionFocus::Reading);
    assert_eq!(tui.app.attachments.len(), 1);
    tui.cleanup_attachments();
}
