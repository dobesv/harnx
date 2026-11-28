//! Regression tests for C5-D3: completion fencing for ALL bound commands
//! (including row-bound `.delete`/`.edit`/`.rewind`), ensuring stale A
//! completions never tear down B's load/epoch/overlay/queue or append A's
//! outcome into B, while same-target and return-to-A reconciliations still work.

use super::cross_session_transcript_action_tests::{
    append_completed_turn, archive_session_picker, open_log_for_session,
    open_session_picker_and_select, recv_history_event, send_key, setup_nats_mutation_test_env,
    submit_input_line,
};
use super::*;
use crate::history_work::RebuildMode;
use crate::types::{TranscriptItem, TuiEvent};
use crossterm::event::{KeyCode, KeyModifiers};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
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

pub(super) fn install_parked_command_gate(
    tui: &mut Tui,
    prefix: &'static str,
) -> (Arc<Notify>, Arc<Notify>) {
    let parked = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let parked_hook = parked.clone();
    let release_hook = release.clone();
    tui.history_command_gate = Some(Arc::new(move |line, _| {
        let parked = parked_hook.clone();
        let release = release_hook.clone();
        Box::pin(async move {
            if line.trim_start().starts_with(prefix) {
                parked.notify_one();
                release.notified().await;
            }
            harnx_runtime::utils::create_abort_signal()
        })
    }));
    (parked, release)
}

fn install_parked_delete_gate(tui: &mut Tui) -> (Arc<Notify>, Arc<Notify>) {
    install_parked_command_gate(tui, ".delete")
}

async fn drain_until(tui: &mut Tui, mut keep_waiting: impl FnMut(&Tui) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while keep_waiting(tui) {
        while let Ok(ev) = tui.event_rx.try_recv() {
            tui.handle_tui_event(ev).await.unwrap();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out draining Tui history work"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

struct BWorkSnapshot {
    epoch: u64,
    view_rev: u64,
    rebuild_seq: u64,
    abort: harnx_runtime::utils::AbortSignal,
}

fn install_gated_b_loader(tui: &mut Tui) -> (Arc<Notify>, watch::Sender<bool>, Arc<AtomicUsize>) {
    let b_started = Arc::new(Notify::new());
    let (release_b, b_gate) = watch::channel(false);
    let b_calls = Arc::new(AtomicUsize::new(0));
    let started_hook = b_started.clone();
    let calls_hook = b_calls.clone();
    tui.history_load_override = Some(Arc::new(move |cfg, _mode, abort| {
        let started_hook = started_hook.clone();
        let mut b_gate = b_gate.clone();
        let calls_hook = calls_hook.clone();
        Box::pin(async move {
            let sid = cfg.read().session.as_ref().map(|s| s.id().to_string());
            assert_eq!(sid.as_deref(), Some("session-b"));
            calls_hook.fetch_add(1, Ordering::SeqCst);
            started_hook.notify_one();
            tokio::select! {
                _ = harnx_core::abort::wait_abort_signal(&abort) => anyhow::bail!("aborted"),
                _ = b_gate.wait_for(|ready| *ready) => {}
            }
            Ok(vec![user_row(1, "B clean row")])
        })
    }));
    (b_started, release_b, b_calls)
}

fn assert_b_work_preserved(
    tui: &Tui,
    snap: &BWorkSnapshot,
    b_calls: &AtomicUsize,
    queue_bound_empty: bool,
) {
    assert_eq!(
        tui.transcript_view_ownership.session_fence.target_epoch,
        snap.epoch
    );
    assert_eq!(b_calls.load(Ordering::SeqCst), 1);
    assert!(tui.app.detail_view_open);
    assert_eq!(tui.app.detail_view_text.as_deref(), Some("B overlay text"));
    assert!(tui.history_command_running);
    if queue_bound_empty {
        let running_b = tui
            .transcript_view_ownership
            .session_fence
            .running_bound_target
            .as_ref()
            .expect("B queued bound command must start instead of being pruned");
        assert_eq!(running_b.target.session_id.as_deref(), Some("session-b"));
        assert_eq!(running_b.target_epoch, snap.epoch);
    } else {
        assert_eq!(tui.transcript_view_ownership.view_revision, snap.view_rev);
        assert_eq!(tui.history_rebuild_seq, snap.rebuild_seq);
        assert!(!snap.abort.aborted());
    }
    assert!(!tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::ErrorText(_)
            | TranscriptItem::MutationNotice(_)
            | TranscriptItem::SystemText(_)
    )));
}

async fn run_bound_delete_across_a_to_b(inject_error: bool, queue_bound_empty: bool) {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript = vec![user_row(1, "A secret row")];
    let (parked, release) = install_parked_delete_gate(&mut tui);
    let (b_started, release_b, b_calls) = install_gated_b_loader(&mut tui);

    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('y'), KeyModifiers::NONE).await;
    parked.notified().await;

    open_session_picker_and_select(&mut tui, "session-b").await;
    b_started.notified().await;
    let queued_cmd = if queue_bound_empty {
        ".empty session"
    } else {
        ".session"
    };
    submit_input_line(&mut tui, queued_cmd).await;
    assert_eq!(tui.history_command_queue.len(), 1);
    tui.app.detail_view_open = true;
    tui.app.detail_view_text = Some("B overlay text".to_string());
    let snap = BWorkSnapshot {
        epoch: tui.transcript_view_ownership.session_fence.target_epoch,
        view_rev: tui.transcript_view_ownership.view_revision,
        rebuild_seq: tui.history_rebuild_seq,
        abort: tui.history_rebuild_abort.clone().expect("B load in flight"),
    };

    release.notify_one();
    let mut a_done = recv_history_event(&mut tui, |e| {
        matches!(e, TuiEvent::HistoryCommandFinished { line, .. } if line.starts_with(".delete"))
    })
    .await;
    if let TuiEvent::HistoryCommandFinished { output, result, .. } = &mut a_done {
        *output = if inject_error { "" } else { "A row deleted" }.to_string();
        *result = if inject_error {
            Err(anyhow::anyhow!("A row mutation failed"))
        } else {
            Ok(harnx_runtime::commands::CommandOutcome::Continue)
        };
    }
    tui.handle_tui_event(a_done).await.unwrap();
    assert_b_work_preserved(&tui, &snap, &b_calls, queue_bound_empty);

    let _ = release_b.send(true);
    drain_until(&mut tui, |t| {
        t.history_command_running || t.history_rebuild_abort.is_some()
    })
    .await;
    tui.flush_history_work_for_test().await;
    assert_eq!(
        tui.actionable_transcript_binding()
            .and_then(|b| b.target.session_id),
        Some("session-b".to_string())
    );
    assert!(!tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::ErrorText(t)
            | TranscriptItem::MutationNotice(t)
            | TranscriptItem::SystemText(t)
            if t.contains("A row")
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_bound_row_completion_preserves_b_work_on_success_and_error() {
    run_bound_delete_across_a_to_b(false, false).await;
    run_bound_delete_across_a_to_b(true, false).await;
    run_bound_delete_across_a_to_b(false, true).await;
    run_bound_delete_across_a_to_b(true, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_target_and_return_a_row_bound_mutations_still_reconcile_only_a() {
    if !super::has_nats_server_binary() {
        return;
    }
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let _nats_env = NatsEnvReset::clear();
    let (config, _session_a, log_a) =
        setup_nats_mutation_test_env(&temp, "archive", "session-a").await;
    append_completed_turn(&log_a, "A turn 1").await;
    append_completed_turn(&log_a, "A turn 2").await;
    let log_b = open_log_for_session(&config, "archive", "session-b").await;
    append_completed_turn(&log_b, "B private turn").await;
    let b_before = log_b.load_events_async().await.unwrap().len();

    let mut tui = Tui::init(&config).await.unwrap();
    tui.history_repair_retry_delay_override = Some(Duration::from_millis(15));
    let (parked, release) = install_parked_delete_gate(&mut tui);

    // 1. Same-target positive: deleting turn 2 user message while staying on A reconciles A.
    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('y'), KeyModifiers::NONE).await;
    parked.notified().await;
    release.notify_one();
    let same_done = recv_history_event(&mut tui, |e| {
        matches!(e, TuiEvent::HistoryCommandFinished { line, .. } if line.starts_with(".delete"))
    })
    .await;
    tui.handle_tui_event(same_done).await.unwrap();
    tui.flush_history_work_for_test().await;
    assert!(!tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::UserText { text, .. } if text == "A turn 2"
    )));
    assert!(tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::UserText { text, .. } if text == "A turn 1"
    )));

    // 2. Return-to-A row-bound reconciliation with failed post-settlement read replaces B rows.
    let a_calls = Arc::new(AtomicUsize::new(0));
    let a_hook = a_calls.clone();
    tui.history_load_override = Some(Arc::new(move |cfg, mode, abort| {
        let a_calls = a_hook.clone();
        Box::pin(async move {
            let sid = cfg.read().session.as_ref().map(|s| s.id().to_string());
            if sid.as_deref() == Some("session-b") {
                return crate::lifecycle::load_session_history_transcript_items(&cfg, Some(&abort))
                    .await;
            }
            if a_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                assert_eq!(mode, RebuildMode::PreserveTail);
                anyhow::bail!("A post-row-delete read failed")
            }
            assert_eq!(mode, RebuildMode::RepairInitial);
            crate::lifecycle::load_session_history_transcript_items(&cfg, Some(&abort)).await
        })
    }));

    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('y'), KeyModifiers::NONE).await;
    parked.notified().await;

    tui.app.modal = Some(archive_session_picker("session-b", "session-a"));
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    let b_seq = tui.history_rebuild_seq;
    let b_done = recv_history_event(
        &mut tui,
        |e| matches!(e, TuiEvent::HistoryRebuildFinished(done) if done.seq == b_seq),
    )
    .await;
    tui.handle_tui_event(b_done).await.unwrap();

    tui.app.modal = Some(archive_session_picker("session-a", "session-b"));
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    assert_eq!(a_calls.load(Ordering::SeqCst), 0);

    release.notify_one();
    let return_done = recv_history_event(&mut tui, |e| {
        matches!(e, TuiEvent::HistoryCommandFinished { line, .. } if line.starts_with(".delete"))
    })
    .await;
    tui.handle_tui_event(return_done).await.unwrap();
    let first_a_seq = tui.history_rebuild_seq;
    let first_a_err = recv_history_event(
        &mut tui,
        |e| matches!(e, TuiEvent::HistoryRebuildFinished(done) if done.seq == first_a_seq),
    )
    .await;
    tui.handle_tui_event(first_a_err).await.unwrap();
    assert!(!tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::UserText { text, .. } if text == "B private turn"
    )));
    assert!(tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::ErrorText(t) if t.contains("A post-row-delete read failed")
    )));

    drain_until(&mut tui, |t| {
        t.is_history_repair_pending() || t.history_rebuild_abort.is_some()
    })
    .await;
    assert_eq!(a_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        tui.actionable_transcript_binding()
            .and_then(|b| b.target.session_id),
        Some("session-a".to_string())
    );
    assert_eq!(log_b.load_events_async().await.unwrap().len(), b_before);
}
