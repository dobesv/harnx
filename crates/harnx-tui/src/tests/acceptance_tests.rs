use super::test_config;
use crate::types::{PendingMessage, Tui, TuiEvent};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_runtime::nats_session::InterruptOutcome;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[tokio::test(start_paused = true)]
async fn accepted_root_reopens_composer_and_starts_g2_without_draining_g1() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.app.llm_busy = true;
    tui.active_remote_session = Some(("session".to_string(), "local".to_string()));
    let old = harnx_runtime::utils::create_abort_signal();
    tui.current_prompt_abort = Some(old.clone());
    tui.current_prompt_handle = Some(tokio::spawn(std::future::pending()));
    tui.set_exit_cancel_factory(Arc::new(|_, _, session_id, cluster| {
        assert_eq!(session_id, "session");
        assert_eq!(cluster, "local");
        Box::pin(async { Ok(InterruptOutcome::Accepted { cancel_seq: 5 }) })
    }));
    tui.start_cancellation("session".into(), "local".into());
    let before = tokio::time::Instant::now();
    tui.poll_pending_exit_cancel().await;
    assert_eq!(tokio::time::Instant::now(), before);
    assert!(!tui.app.llm_busy);
    assert!(tui.cancellation.is_none() && tui.app.modal.is_none());
    assert!(tui.current_prompt_handle.is_none());
    tui.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(tui.app.input.lines(), &["x"]);
    tui.start_prompt(PendingMessage {
        text: "G2".into(),
        attachments: vec![],
        attachment_dir: None,
        paste_count: 0,
    })
    .await
    .unwrap();
    assert_eq!(tokio::time::Instant::now(), before);
    let new = tui.current_prompt_abort.clone().unwrap();
    assert!(!Arc::ptr_eq(&new, &old));
    tui.handle_tui_event(TuiEvent::PromptTaskFinished {
        task: old,
        error: Some("late G1 failure".into()),
    })
    .await
    .unwrap();
    assert!(tui.app.llm_busy);
    assert!(Arc::ptr_eq(
        tui.current_prompt_abort.as_ref().unwrap(),
        &new
    ));
    tui.retire_prompt_task();
}

#[tokio::test]
async fn queued_confirmation_from_retired_route_cannot_open_g2_modal() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    let closed = Arc::new(AtomicBool::new(false));
    let handler =
        crate::lifecycle::nats_tool_confirmation_handler(tui.event_tx.clone(), closed.clone());
    let task = tokio::spawn(handler(
        harnx_runtime::nats_tool_confirmation::ToolConfirmationRequest {
            session_id: "session".into(),
            tool_call_id: Some("g1-tool".into()),
            tool_name: "bash".into(),
            arguments: serde_json::json!({}),
            reason: None,
        },
    ));
    let event = tui.event_rx.recv().await.unwrap();
    // Match route.shutdown's synchronous boundary, without waiting for its responder.
    closed.store(true, Ordering::Release);
    tui.app.llm_busy = true;
    let before = tui.app.transcript.len();
    tui.handle_tui_event(event).await.unwrap();
    assert!(tui.app.modal.is_none());
    assert_eq!(tui.app.transcript.len(), before);
    assert!(!task.await.unwrap());
}

#[tokio::test]
async fn prompt_cancellation_target_uses_agent_scoped_storage_identity() {
    let config = super::test_config_with_mock_client_and_agent("metis", Some("same-local-id"));
    let mut tui = Tui::init(&config).await.unwrap();
    let expected = config.read().session.as_ref().unwrap().storage_key();
    let other = harnx_core::session_identity::session_key(Some("other-agent"), "same-local-id");
    assert_ne!(expected, other);
    assert_eq!(tui.session_activity_destination().unwrap().0, expected);
    tui.start_prompt(PendingMessage {
        text: "prompt".into(),
        attachments: vec![],
        attachment_dir: None,
        paste_count: 0,
    })
    .await
    .unwrap();
    assert_eq!(tui.active_remote_session.as_ref().unwrap().0, expected);
    tui.retire_prompt_task();
}
