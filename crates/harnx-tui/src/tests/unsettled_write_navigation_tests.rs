//! Regression tests for C5-D1, C5-D2, and C5-T1:
//! - Full durable-destination unsettled-write exclusion across A -> B -> A while
//!   allowing unrelated B reads and blocking pre-write A snapshots until settlement.
//! - Preserving navigation replacement intent through stale A settlement so
//!   failed post-settlement A reads replace B rows under A chrome.
//! - Full destination (not short ID) comparison in stale-command reconciliation.

use super::cross_session_transcript_action_tests::{
    active_session_id, append_completed_turn, archive_session_picker, open_log_for_session,
    recv_history_event, send_key, setup_nats_mutation_test_env, submit_input_line,
};
use super::*;
use crate::history_work::{RebuildFailurePolicy, RebuildMode};
use crate::types::{
    HistoryDestination, HistoryTarget, SessionCommandTarget, TranscriptItem, TuiEvent,
};
use crossterm::event::{KeyCode, KeyModifiers};
use harnx_core::session::SessionLogEntry;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::Notify;

fn install_parked_empty_command_gate(tui: &mut Tui) -> (Arc<Notify>, Arc<Notify>, Arc<AtomicBool>) {
    let parked = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let saw_abort = Arc::new(AtomicBool::new(false));
    let parked_hook = parked.clone();
    let release_hook = release.clone();
    let abort_hook = saw_abort.clone();
    tui.history_command_gate = Some(Arc::new(move |line, abort_signal| {
        let parked = parked_hook.clone();
        let release = release_hook.clone();
        let saw_abort = abort_hook.clone();
        Box::pin(async move {
            if line.trim_start().starts_with(".empty") {
                parked.notify_one();
                release.notified().await;
                saw_abort.store(abort_signal.aborted(), Ordering::SeqCst);
            }
            harnx_runtime::utils::create_abort_signal()
        })
    }));
    (parked, release, saw_abort)
}

fn install_return_a_repair_loader(tui: &mut Tui) -> Arc<AtomicUsize> {
    let a_attempts = Arc::new(AtomicUsize::new(0));
    let a_hook = a_attempts.clone();
    tui.history_load_override = Some(Arc::new(move |cfg, mode, abort| {
        let a_attempts = a_hook.clone();
        Box::pin(async move {
            let sid = cfg.read().session.as_ref().map(|s| s.id().to_string());
            if sid.as_deref() == Some("session-b") {
                return crate::lifecycle::load_session_history_transcript_items(&cfg, Some(&abort))
                    .await;
            }
            assert_eq!(sid.as_deref(), Some("session-a"));
            match a_attempts.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    assert_eq!(mode, RebuildMode::PreserveTail);
                    anyhow::bail!("session A post-write read failed 1")
                }
                1 => {
                    assert_eq!(mode, RebuildMode::RepairInitial);
                    anyhow::bail!("session A post-write read failed 2")
                }
                _ => {
                    assert_eq!(mode, RebuildMode::RepairInitial);
                    crate::lifecycle::load_session_history_transcript_items(&cfg, Some(&abort))
                        .await
                }
            }
        })
    }));
    a_attempts
}

fn assert_no_b_or_prewrite_a_rows(tui: &Tui) {
    assert!(!tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::UserText { text, .. }
            if text == "B secret prompt" || text == "A original prompt"
    )));
    assert!(!tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::AssistantText { text, .. }
            if text == "Reply to B secret prompt" || text == "Reply to A original prompt"
    )));
    assert!(!tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::SystemText(t) if t.contains("session-b")
    )));
}

fn assert_a_error_chrome_without_b_or_prewrite_a(tui: &Tui, needle: &str) {
    assert_no_b_or_prewrite_a_rows(tui);
    assert!(tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::SystemText(t) if t.contains("session-a")
    )));
    assert!(tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::ErrorText(t) if t.contains(needle)
    )));
    assert!(tui.actionable_transcript_binding().is_none());
}

pub(super) async fn drain_history_repair_to_completion(tui: &mut Tui) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while tui.is_history_repair_pending() || tui.history_rebuild_abort.is_some() {
        while let Ok(ev) = tui.event_rx.try_recv() {
            tui.handle_tui_event(ev).await.unwrap();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for A post-settlement repair"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn assert_pre_settlement_a_read_blocked(tui: &mut Tui, a_attempts: &AtomicUsize) {
    assert_eq!(active_session_id(tui).as_deref(), Some("session-a"));
    assert!(tui.actionable_transcript_binding().is_none());
    assert!(tui.history_rebuild_abort.is_none());
    assert_eq!(a_attempts.load(Ordering::SeqCst), 0);

    tui.schedule_history_rebuild();
    assert!(tui.history_rebuild_abort.is_none());
    assert_eq!(a_attempts.load(Ordering::SeqCst), 0);

    let prewrite = new_history_rebuild_completion(
        tui,
        RebuildMode::PreserveTail,
        RebuildFailurePolicy::ReplaceWithInitialErrorAndRepair,
        Ok(vec![TranscriptItem::UserText {
            text: "A original prompt".to_string(),
            seq: Some(1),
            timestamp: None,
        }]),
    );
    tui.handle_tui_event(TuiEvent::HistoryRebuildFinished(Box::new(prewrite)))
        .await
        .unwrap();
    assert!(tui.actionable_transcript_binding().is_none());
    assert!(!tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::UserText { text, .. } if text == "A original prompt"
    )));

    send_key(tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    assert!(tui.app.modal.is_none());
    send_key(tui, KeyCode::Esc, KeyModifiers::NONE).await;
    submit_input_line(tui, ".delete message 1").await;
    assert!(tui.history_command_queue.is_empty());
}

async fn drive_post_settlement_a_failure_and_repair(tui: &mut Tui, a_attempts: &AtomicUsize) {
    let first_seq = tui.history_rebuild_seq;
    let first_err = recv_history_event(
        tui,
        |e| matches!(e, TuiEvent::HistoryRebuildFinished(done) if done.seq == first_seq),
    )
    .await;
    tui.handle_tui_event(first_err).await.unwrap();
    assert_a_error_chrome_without_b_or_prewrite_a(tui, "session A post-write read failed 1");

    let retry =
        recv_history_event(tui, |e| matches!(e, TuiEvent::HistoryRepairRetryDue { .. })).await;
    tui.handle_tui_event(retry).await.unwrap();
    let second_seq = tui.history_rebuild_seq;
    let second_err = recv_history_event(
        tui,
        |e| matches!(e, TuiEvent::HistoryRebuildFinished(done) if done.seq == second_seq),
    )
    .await;
    tui.handle_tui_event(second_err).await.unwrap();
    assert_a_error_chrome_without_b_or_prewrite_a(tui, "session A post-write read failed 2");

    drain_history_repair_to_completion(tui).await;
    assert_eq!(a_attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parked_a_write_navigation_a_to_b_to_a_blocks_prewrite_rearm_and_replaces_b_rows_on_postsettlement_failure_repair(
) {
    if !super::has_nats_server_binary() {
        return;
    }
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let _nats_env = NatsEnvReset::clear();
    let (config, _session_a, log_a) =
        setup_nats_mutation_test_env(&temp, "archive", "session-a").await;
    append_completed_turn(&log_a, "A original prompt").await;
    let a_before = log_a.load_events_async().await.unwrap().len();
    let log_b = open_log_for_session(&config, "archive", "session-b").await;
    append_completed_turn(&log_b, "B secret prompt").await;
    let b_before = log_b.load_events_async().await.unwrap().len();

    let mut tui = Tui::init(&config).await.unwrap();
    tui.history_repair_retry_delay_override = Some(Duration::from_millis(15));
    let (parked, release, saw_abort) = install_parked_empty_command_gate(&mut tui);
    let a_attempts = install_return_a_repair_loader(&mut tui);

    submit_input_line(&mut tui, ".empty  session").await;
    parked.notified().await;
    assert_eq!(log_a.load_events_async().await.unwrap().len(), a_before);

    // Unrelated B read succeeds while A write is parked.
    tui.app.modal = Some(archive_session_picker("session-b", "session-a"));
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    let b_seq = tui.history_rebuild_seq;
    let b_done = recv_history_event(
        &mut tui,
        |e| matches!(e, TuiEvent::HistoryRebuildFinished(done) if done.seq == b_seq),
    )
    .await;
    tui.handle_tui_event(b_done).await.unwrap();
    assert_eq!(
        tui.actionable_transcript_binding()
            .expect("B must bind while A write is parked")
            .target
            .session_id
            .as_deref(),
        Some("session-b")
    );

    // Return B -> A while A write is still parked: pre-write reads/rearms stay blocked.
    tui.app.modal = Some(archive_session_picker("session-a", "session-b"));
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    assert_pre_settlement_a_read_blocked(&mut tui, &a_attempts).await;

    // Release parked A write -> stale completion settles and launches post-settlement A read.
    release.notify_one();
    let settled = recv_history_event(&mut tui, |e| {
        matches!(e, TuiEvent::HistoryCommandFinished { result: Ok(_), .. })
    })
    .await;
    assert!(saw_abort.load(Ordering::SeqCst));
    tui.handle_tui_event(settled).await.unwrap();
    assert!(tui.history_rebuild_abort.is_some());

    drive_post_settlement_a_failure_and_repair(&mut tui, &a_attempts).await;

    assert!(!tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::UserText { text, .. }
            if text == "B secret prompt" || text == "A original prompt"
    )));
    let binding = tui
        .actionable_transcript_binding()
        .expect("post-settlement repair must bind session-a");
    assert_eq!(binding.target.session_id.as_deref(), Some("session-a"));

    let a_after = log_a.load_events_async().await.unwrap();
    assert_eq!(a_after.len(), a_before + 1);
    assert!(matches!(
        a_after.last().map(|(_, e)| e),
        Some(SessionLogEntry::Clear)
    ));
    assert_eq!(log_b.load_events_async().await.unwrap().len(), b_before);
}

fn install_reconnect_supersession_a_repair_loader(
    tui: &mut Tui,
) -> (Arc<AtomicUsize>, Arc<Notify>, Arc<Notify>) {
    let a_attempts = Arc::new(AtomicUsize::new(0));
    let first_read_parked = Arc::new(Notify::new());
    let first_read_release = Arc::new(Notify::new());
    let a_hook = a_attempts.clone();
    let parked_hook = first_read_parked.clone();
    let release_hook = first_read_release.clone();
    tui.history_load_override = Some(Arc::new(move |cfg, mode, abort| {
        let a_attempts = a_hook.clone();
        let first_read_parked = parked_hook.clone();
        let first_read_release = release_hook.clone();
        Box::pin(async move {
            let sid = cfg.read().session.as_ref().map(|s| s.id().to_string());
            if sid.as_deref() == Some("session-b") {
                return crate::lifecycle::load_session_history_transcript_items(&cfg, Some(&abort))
                    .await;
            }
            assert_eq!(sid.as_deref(), Some("session-a"));
            match a_attempts.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    assert_eq!(mode, RebuildMode::PreserveTail);
                    first_read_parked.notify_one();
                    first_read_release.notified().await;
                    anyhow::bail!("session A initial return read superseded")
                }
                1 => {
                    assert_eq!(mode, RebuildMode::RepairInitial);
                    anyhow::bail!("session A superseding repair failed 1")
                }
                2 => {
                    assert_eq!(mode, RebuildMode::RepairInitial);
                    anyhow::bail!("session A superseding repair failed 2")
                }
                _ => {
                    assert_eq!(mode, RebuildMode::RepairInitial);
                    crate::lifecycle::load_session_history_transcript_items(&cfg, Some(&abort))
                        .await
                }
            }
        })
    }));
    (a_attempts, first_read_parked, first_read_release)
}

async fn navigate_a_to_b_to_a_while_parked(tui: &mut Tui, a_attempts: &AtomicUsize) {
    // Navigate A -> B while A write is parked; B read commits and binds.
    tui.app.modal = Some(archive_session_picker("session-b", "session-a"));
    send_key(tui, KeyCode::Enter, KeyModifiers::NONE).await;
    let b_seq = tui.history_rebuild_seq;
    let b_done = recv_history_event(
        tui,
        |e| matches!(e, TuiEvent::HistoryRebuildFinished(done) if done.seq == b_seq),
    )
    .await;
    tui.handle_tui_event(b_done).await.unwrap();
    assert_eq!(
        tui.actionable_transcript_binding()
            .expect("B must bind while A write is parked")
            .target
            .session_id
            .as_deref(),
        Some("session-b")
    );

    // Return B -> A while A write is still parked; pre-settlement A read stays blocked.
    tui.app.modal = Some(archive_session_picker("session-a", "session-b"));
    send_key(tui, KeyCode::Enter, KeyModifiers::NONE).await;
    assert_pre_settlement_a_read_blocked(tui, a_attempts).await;
}

async fn drive_reconnect_supersession_failures_and_recovery(
    tui: &mut Tui,
    target_a: &(String, String),
    first_read_release: &Notify,
    a_attempts: &AtomicUsize,
) {
    let pre_reconnect_seq = tui.history_rebuild_seq;
    let pre_reconnect_gen = tui.session_attachment_gen;
    let repeat_attach = super::history_repair_tests::stream_attached_event(tui, target_a, false);
    tui.handle_tui_event(repeat_attach).await.unwrap();
    assert!(tui.session_attachment_gen > pre_reconnect_gen);
    assert!(tui.history_rebuild_seq > pre_reconnect_seq);
    assert!(tui.history_rebuild_abort.is_some());
    first_read_release.notify_one();

    // Superseding RepairInitial fails -> must replace B rows under A chrome, not preserve B.
    let superseding_seq = tui.history_rebuild_seq;
    let superseding_err = recv_history_event(
        tui,
        |e| matches!(e, TuiEvent::HistoryRebuildFinished(done) if done.seq == superseding_seq),
    )
    .await;
    assert!(matches!(
        &superseding_err,
        TuiEvent::HistoryRebuildFinished(done) if done.mode == RebuildMode::RepairInitial
    ));
    tui.handle_tui_event(superseding_err).await.unwrap();
    assert_a_error_chrome_without_b_or_prewrite_a(tui, "session A superseding repair failed 1");

    // Next paced RepairInitial retry fails -> must still show no B rows and stay unactionable.
    let retry =
        recv_history_event(tui, |e| matches!(e, TuiEvent::HistoryRepairRetryDue { .. })).await;
    tui.handle_tui_event(retry).await.unwrap();
    let retry_seq = tui.history_rebuild_seq;
    let retry_err = recv_history_event(
        tui,
        |e| matches!(e, TuiEvent::HistoryRebuildFinished(done) if done.seq == retry_seq),
    )
    .await;
    assert!(matches!(
        &retry_err,
        TuiEvent::HistoryRebuildFinished(done) if done.mode == RebuildMode::RepairInitial
    ));
    tui.handle_tui_event(retry_err).await.unwrap();
    assert_a_error_chrome_without_b_or_prewrite_a(tui, "session A superseding repair failed 2");

    drain_history_repair_to_completion(tui).await;
    assert_eq!(a_attempts.load(Ordering::SeqCst), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parked_a_write_navigation_a_to_b_to_a_reconnect_repair_supersession_replaces_b_rows_until_fresh_a_commit(
) {
    if !super::has_nats_server_binary() {
        return;
    }
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let _nats_env = NatsEnvReset::clear();
    let (config, _session_a, log_a) =
        setup_nats_mutation_test_env(&temp, "archive", "session-a").await;
    append_completed_turn(&log_a, "A original prompt").await;
    let a_before = log_a.load_events_async().await.unwrap().len();
    let log_b = open_log_for_session(&config, "archive", "session-b").await;
    append_completed_turn(&log_b, "B secret prompt").await;
    let b_before = log_b.load_events_async().await.unwrap().len();

    let mut tui = Tui::init(&config).await.unwrap();
    tui.history_repair_retry_delay_override = Some(Duration::from_millis(15));
    let (parked, release, saw_abort) = install_parked_empty_command_gate(&mut tui);
    let (a_attempts, first_read_parked, first_read_release) =
        install_reconnect_supersession_a_repair_loader(&mut tui);

    submit_input_line(&mut tui, ".empty  session").await;
    parked.notified().await;
    assert_eq!(log_a.load_events_async().await.unwrap().len(), a_before);

    navigate_a_to_b_to_a_while_parked(&mut tui, &a_attempts).await;

    let target_a = tui
        .session_activity_target
        .clone()
        .expect("current A monitor target");
    let first_attach = recv_history_event(&mut tui, |e| {
        matches!(
            e,
            TuiEvent::SessionStreamAttached { session_id, .. } if session_id == &target_a.0
        )
    })
    .await;
    tui.handle_tui_event(first_attach).await.unwrap();
    assert!(tui.session_stream_attached);
    assert!(tui.history_rebuild_abort.is_none());
    assert_eq!(a_attempts.load(Ordering::SeqCst), 0);

    release.notify_one();
    let settled = recv_history_event(&mut tui, |e| {
        matches!(e, TuiEvent::HistoryCommandFinished { result: Ok(_), .. })
    })
    .await;
    assert!(saw_abort.load(Ordering::SeqCst));
    tui.handle_tui_event(settled).await.unwrap();
    assert!(tui.history_rebuild_abort.is_some());
    assert!(tui.is_history_repair_pending());
    first_read_parked.notified().await;
    assert!(tui.actionable_transcript_binding().is_none());

    drive_reconnect_supersession_failures_and_recovery(
        &mut tui,
        &target_a,
        &first_read_release,
        &a_attempts,
    )
    .await;

    assert_no_b_or_prewrite_a_rows(&tui);
    let binding = tui
        .actionable_transcript_binding()
        .expect("fresh A repair must bind session-a");
    assert_eq!(binding.target.session_id.as_deref(), Some("session-a"));
    let a_after = log_a.load_events_async().await.unwrap();
    assert_eq!(a_after.len(), a_before + 1);
    assert!(matches!(
        a_after.last().map(|(_, e)| e),
        Some(SessionLogEntry::Clear)
    ));
    assert_eq!(log_b.load_events_async().await.unwrap().len(), b_before);
}

fn session_command_target(dest: &HistoryDestination, target_epoch: u64) -> SessionCommandTarget {
    SessionCommandTarget {
        target: dest.target.clone(),
        storage_key: dest.storage_key.clone(),
        target_epoch,
    }
}

#[tokio::test]
async fn stale_completion_requires_full_destination_match_not_short_ids() {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    let current_dest = tui.current_history_destination();
    tui.transcript_view_ownership.unsettled_write_destination = Some(current_dest.clone());

    // Same short session_id/agent, different cluster + storage_key must not match current destination.
    let other_dest = HistoryDestination {
        target: HistoryTarget {
            session_id: current_dest.target.session_id.clone(),
            agent: current_dest.target.agent.clone(),
            cluster: "other-cluster".to_string(),
        },
        storage_key: Some("other-storage-key".to_string()),
    };
    let current_epoch = tui.transcript_view_ownership.session_fence.target_epoch;
    tui.transcript_view_ownership
        .session_fence
        .running_bound_target = Some(session_command_target(&other_dest, current_epoch));
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_hook = calls.clone();
    tui.history_load_override = Some(Arc::new(move |_cfg, _mode, _abort| {
        calls_hook.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(vec![]) })
    }));

    tui.finish_history_command(
        ".empty session".to_string(),
        (
            current_dest.target.session_id.clone(),
            current_dest.target.agent.clone(),
        ),
        String::new(),
        Ok(harnx_runtime::commands::CommandOutcome::Continue),
    )
    .await;
    tui.flush_history_work_for_test().await;
    assert_eq!(
        tui.transcript_view_ownership
            .unsettled_write_destination
            .as_ref(),
        Some(&current_dest)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // Matching full destination (even from an earlier navigation epoch) settles and reconciles.
    tui.transcript_view_ownership
        .session_fence
        .running_bound_target = Some(session_command_target(
        &current_dest,
        current_epoch.wrapping_sub(1),
    ));
    tui.finish_history_command(
        ".empty session".to_string(),
        (
            current_dest.target.session_id.clone(),
            current_dest.target.agent.clone(),
        ),
        String::new(),
        Ok(harnx_runtime::commands::CommandOutcome::Continue),
    )
    .await;
    tui.flush_history_work_for_test().await;
    assert_eq!(
        tui.transcript_view_ownership.unsettled_write_destination,
        None
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
