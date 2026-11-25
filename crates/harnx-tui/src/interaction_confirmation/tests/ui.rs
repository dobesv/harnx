use super::{drawn_screen, drawn_screen_at_tiny_sizes, modal, snapshot, tui_with_pending};
use crate::interaction_confirmation::{InteractionEvent, ModalState};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[tokio::test]
async fn modal_renders_public_message_and_unsupported_kind_without_approve() {
    let mut tui = tui_with_pending().await;
    let mut state = modal(&mut tui).clone();
    let screen = drawn_screen(&mut tui, &mut state, 100, 12);
    assert!(screen.contains("Allow second?"));
    assert!(screen.contains("1 more pending"));
    assert!(screen.contains("ENTER approve"));
    assert!(!screen.contains("private"));
    state.pending.kind = "future-form".into();
    let screen = drawn_screen(&mut tui, &mut state, 100, 12);
    assert!(screen.contains("not supported by this client"));
    assert!(screen.contains("Ctrl+D deny"));
    assert!(!screen.contains("ENTER approve"));
    drawn_screen_at_tiny_sizes(&mut tui, &mut state);
}

#[tokio::test]
async fn paste_and_typing_edit_only_note_and_enter_is_idle_gated() {
    let mut tui = tui_with_pending().await;
    tui.set_input_text("preserve composer");
    tui.handle_paste("note\r\nsecond line".into()).await;
    assert_eq!(modal(&mut tui).note.lines().join("\n"), "note\nsecond line");
    assert_eq!(tui.app.input.lines().join("\n"), "preserve composer");
    assert!(tui.app.attachments.is_empty());
    tui.handle_interaction_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE))
        .await;
    tui.handle_interaction_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert!(
        !modal(&mut tui).submitting,
        "fresh typing cannot accidentally approve"
    );
    assert_eq!(
        modal(&mut tui).note.lines().join("\n"),
        "note\nsecond line!"
    );
    modal(&mut tui).submitting = true;
    tui.handle_paste("must not edit while sending".into()).await;
    assert_eq!(
        modal(&mut tui).note.lines().join("\n"),
        "note\nsecond line!"
    );
}

#[tokio::test]
async fn snapshots_preserve_note_and_generation_fences_old_session() {
    let mut tui = tui_with_pending().await;
    let head = modal(&mut tui).pending.clone();
    modal(&mut tui).note.insert_str("draft");
    snapshot(&mut tui, Some(head.clone()));
    assert_eq!(modal(&mut tui).note.lines().join("\n"), "draft");
    tui.interactions.generation += 1;
    tui.app.modal = None;
    tui.interactions.pending = None;
    tui.handle_interaction_event(InteractionEvent::Snapshot {
        generation: tui.interactions.generation - 1,
        pending: Ok(Some(head)),
    });
    assert!(tui.app.modal.is_none());
    assert!(tui.interactions.pending.is_none());
}

#[tokio::test]
async fn read_errors_block_response_until_repaired_and_are_visible() {
    let mut tui = tui_with_pending().await;
    let pending = modal(&mut tui).pending.clone();
    tui.handle_interaction_event(InteractionEvent::Snapshot {
        generation: tui.interactions.generation,
        pending: Err("broker offline".into()),
    });
    tui.submit_interaction(false);
    assert!(!modal(&mut tui).submitting);
    assert!(modal(&mut tui)
        .error
        .as_ref()
        .unwrap()
        .contains("broker offline"));
    assert!(tui.interactions.decisions.is_empty());
    snapshot(&mut tui, Some(pending));
    assert!(tui.interactions.read_error.is_none());
    assert!(matches!(
        tui.app.modal,
        Some(ModalState::ConfirmInteraction(_))
    ));
}

#[tokio::test]
async fn ctrl_c_replaces_modal_with_existing_durable_cancellation_tray() {
    let mut tui = tui_with_pending().await;
    tui.exit_cancel_factory = std::sync::Arc::new(|_, _, _, _| Box::pin(std::future::pending()));
    assert!(
        tui.active_remote_session.is_none(),
        "observer can beat activity monitor"
    );
    tui.handle_interaction_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await;
    assert!(tui.app.modal.is_none());
    assert!(tui.has_root_cancellation());
    tui.present_interaction();
    assert!(
        tui.app.modal.is_none(),
        "cancelling request cannot reopen its modal"
    );
}

#[tokio::test]
async fn missing_target_reports_error_without_leaving_submission_stuck() {
    let mut tui = tui_with_pending().await;
    tui.interactions.target = None;
    tui.submit_interaction(false);
    assert!(!modal(&mut tui).submitting);
    assert_eq!(
        modal(&mut tui).error.as_deref(),
        Some("Session is no longer selected")
    );
    assert!(tui.interactions.decisions.is_empty());
}
