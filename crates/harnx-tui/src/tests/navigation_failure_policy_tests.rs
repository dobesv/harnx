//! Regression tests for C4-F5: target-switch reload failure presentation
//! after an in-flight coordinate-invalidating mutation on the prior session.

use super::*;
use crate::history_work::RebuildMode;
use crate::types::{ModalState, TranscriptItem, TuiEvent};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tokio::sync::{watch, Notify};

fn user_row(seq: usize, text: &str) -> TranscriptItem {
    TranscriptItem::UserText {
        text: text.to_string(),
        seq: Some(seq),
        timestamp: None,
    }
}

async fn send_key(tui: &mut Tui, code: KeyCode, modifiers: KeyModifiers) {
    Box::pin(tui.handle_key(KeyEvent::new(code, modifiers)))
        .await
        .unwrap();
}

async fn recv_event(tui: &mut Tui, matches: impl Fn(&TuiEvent) -> bool) -> TuiEvent {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = tui.event_rx.recv().await.unwrap();
            if matches(&event) {
                return event;
            }
        }
    })
    .await
    .expect("missing TuiEvent")
}

fn install_gated_b_repair_loader(
    tui: &mut Tui,
) -> (Arc<Notify>, watch::Sender<bool>, Arc<AtomicUsize>) {
    let load_started = Arc::new(Notify::new());
    let (release_tx, release_rx) = watch::channel(false);
    let attempts = Arc::new(AtomicUsize::new(0));
    let started_hook = load_started.clone();
    let attempts_hook = attempts.clone();
    tui.history_load_override = Some(Arc::new(move |cfg, mode, abort| {
        let started_hook = started_hook.clone();
        let mut release_rx = release_rx.clone();
        let attempts = attempts_hook.clone();
        Box::pin(async move {
            let sid = cfg.read().session.as_ref().map(|s| s.id().to_string());
            assert_eq!(sid.as_deref(), Some("session-b"));
            started_hook.notify_one();
            tokio::select! {
                _ = harnx_core::abort::wait_abort_signal(&abort) => {
                    anyhow::bail!("history load aborted")
                }
                _ = release_rx.wait_for(|ready| *ready) => {}
            }
            match attempts.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    assert_eq!(mode, RebuildMode::PreserveTail);
                    anyhow::bail!("session B read unavailable 1")
                }
                1 => {
                    assert_eq!(mode, RebuildMode::RepairInitial);
                    anyhow::bail!("session B read unavailable 2")
                }
                _ => {
                    assert_eq!(mode, RebuildMode::RepairInitial);
                    Ok(vec![user_row(1, "Session B recovered row")])
                }
            }
        })
    }));
    (load_started, release_tx, attempts)
}

async fn assert_b_error_state_without_a_rows(
    tui: &mut Tui,
    recorded: &Arc<Mutex<Vec<String>>>,
    error_needle: &str,
) {
    assert!(!tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::UserText { text, .. } if text == "Session A secret prompt"
    )));
    assert!(tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::SystemText(t) if t.contains("session-b")
    )));
    assert!(tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::ErrorText(t) if t.contains(error_needle)
    )));
    assert!(tui.actionable_transcript_binding().is_none());
    send_key(tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    assert!(tui.app.modal.is_none());
    send_key(tui, KeyCode::Esc, KeyModifiers::NONE).await;
    tui.set_input_text(".delete message 1");
    send_key(tui, KeyCode::Enter, KeyModifiers::NONE).await;
    assert!(recorded.lock().unwrap().is_empty());
    assert!(tui.is_history_repair_pending());
}

async fn trigger_a_delete_and_switch_to_b(tui: &mut Tui, load_started: &Notify) {
    send_key(tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    assert!(matches!(
        tui.app.modal,
        Some(ModalState::ConfirmDelete { from: 1, to: 1, .. })
    ));
    send_key(tui, KeyCode::Char('y'), KeyModifiers::NONE).await;
    assert!(tui.history_command_running);
    assert!(tui.is_unsettled_write_blocking_current_target());

    super::cross_session_transcript_action_tests::open_session_picker_and_select(tui, "session-b")
        .await;
    let active = tui
        .config
        .read()
        .session
        .as_ref()
        .map(|s| s.id().to_string());
    assert_eq!(active.as_deref(), Some("session-b"));
    load_started.notified().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inflight_mutation_navigation_failure_replaces_a_rows_under_b_chrome_and_repairs_b_only() {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript = vec![user_row(1, "Session A secret prompt")];
    tui.history_repair_retry_delay_override = Some(Duration::from_millis(15));
    let (load_started, release_tx, attempts) = install_gated_b_repair_loader(&mut tui);

    trigger_a_delete_and_switch_to_b(&mut tui, &load_started).await;
    let expected_seq = tui.history_rebuild_seq;

    let a_settled = recv_event(&mut tui, |e| {
        matches!(e, TuiEvent::HistoryCommandFinished { .. })
    })
    .await;
    tui.handle_tui_event(a_settled).await.unwrap();
    assert_eq!(tui.history_rebuild_seq, expected_seq);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    assert!(!tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::ErrorText(_) | TranscriptItem::MutationNotice(_)
    )));
    let _ = release_tx.send(true);

    let first_b_failure = recv_event(
        &mut tui,
        |e| matches!(e, TuiEvent::HistoryRebuildFinished(done) if done.seq == expected_seq),
    )
    .await;
    tui.handle_tui_event(first_b_failure).await.unwrap();

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let sink = recorded.clone();
    tui.command_override = Some(Arc::new(move |line| {
        sink.lock().unwrap().push(line.to_string());
    }));
    assert_b_error_state_without_a_rows(&mut tui, &recorded, "session B read unavailable 1").await;

    let retry_1 = recv_event(&mut tui, |e| {
        matches!(e, TuiEvent::HistoryRepairRetryDue { .. })
    })
    .await;
    tui.handle_tui_event(retry_1).await.unwrap();
    let second_seq = tui.history_rebuild_seq;
    let second_b_failure = recv_event(
        &mut tui,
        |e| matches!(e, TuiEvent::HistoryRebuildFinished(done) if done.seq == second_seq),
    )
    .await;
    tui.handle_tui_event(second_b_failure).await.unwrap();
    assert_b_error_state_without_a_rows(&mut tui, &recorded, "session B read unavailable 2").await;

    super::unsettled_write_navigation_tests::drain_history_repair_to_completion(&mut tui).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert!(!tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::UserText { text, .. } if text == "Session A secret prompt"
    )));
    assert!(tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::UserText { text, .. } if text == "Session B recovered row"
    )));
    let binding = tui
        .actionable_transcript_binding()
        .expect("repaired B view must become actionable");
    assert_eq!(binding.target.session_id.as_deref(), Some("session-b"));

    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('y'), KeyModifiers::NONE).await;
    assert_eq!(recorded.lock().unwrap().as_slice(), [".delete message 1"]);
}
