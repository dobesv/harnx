//! Regression tests for B3: explicit per-rebuild failure policy.
//!
//! Exercises actual UI key/event dispatch (`handle_key`, `handle_tui_event`)
//! across handoff failure -> paced retry -> success/empty-success, post-durable
//! mutation failure -> stale row preservation + action gating -> retry failure
//! -> recovery, compaction rebuild failure, silent shared-refresh failures, and
//! exponential backoff pacing.

use super::*;
use crate::history_work::{RebuildFailurePolicy, RebuildMode, INITIAL_HISTORY_REPAIR_DELAY};
use crate::types::{ModalState, TranscriptItem, TuiEvent};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_core::event::{AgentEvent, SessionEvent};
use harnx_core::session::CompactOutcome;
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

fn assistant_row(seq: usize, text: &str) -> TranscriptItem {
    TranscriptItem::AssistantText {
        text: text.to_string(),
        seq: Some(seq),
        timestamp: None,
        rendered_cache: None,
    }
}

async fn send_key(tui: &mut Tui, code: KeyCode, modifiers: KeyModifiers) {
    tui.handle_key(KeyEvent::new(code, modifiers))
        .await
        .unwrap();
}

async fn submit_input_line(tui: &mut Tui, text: &str) {
    tui.set_input_text(text);
    send_key(tui, KeyCode::Enter, KeyModifiers::NONE).await;
}

fn record_dispatched_commands(tui: &mut Tui) -> Arc<Mutex<Vec<String>>> {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let sink = recorded.clone();
    tui.command_override = Some(Arc::new(move |line| {
        sink.lock().unwrap().push(line.to_string());
    }));
    recorded
}

async fn wait_for_attempt_and_idle(tui: &mut Tui, attempts: &AtomicUsize, min_attempts: usize) {
    const BUDGET: Duration = Duration::from_secs(5);
    let started = std::time::Instant::now();
    loop {
        while let Ok(event) = tui.event_rx.try_recv() {
            tui.handle_tui_event(event).await.unwrap();
        }
        if attempts.load(Ordering::SeqCst) >= min_attempts && tui.history_rebuild_abort.is_none() {
            return;
        }
        assert!(
            started.elapsed() < BUDGET,
            "timed out waiting for rebuild attempt {min_attempts}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn assert_row_actions_rejected(
    tui: &mut Tui,
    recorded: &Mutex<Vec<String>>,
    expect_key_notice: bool,
    expect_cmd_notice: bool,
    ctx: &str,
) {
    assert!(
        tui.actionable_transcript_binding().is_none(),
        "{ctx}: active_binding must remain None while rows are stale/unloaded"
    );
    let dispatched_before = recorded.lock().unwrap().len();
    send_key(tui, KeyCode::Up, KeyModifiers::NONE).await;
    for key in ['d', 'r', 'e'] {
        send_key(tui, KeyCode::Char(key), KeyModifiers::NONE).await;
        assert!(
            tui.app.modal.is_none(),
            "{ctx}: row action key '{key}' must not open modal while repair is pending"
        );
        if expect_key_notice {
            assert!(
                tui.app.footer_notice_text().is_some(),
                "{ctx}: row action key '{key}' must surface stale-transcript notice"
            );
        }
    }
    send_key(tui, KeyCode::Esc, KeyModifiers::NONE).await;
    for cmd in [".delete message 1", ".rewind 1", ".edit message 1"] {
        submit_input_line(tui, cmd).await;
        if expect_cmd_notice {
            assert!(
                tui.app.footer_notice_text().is_some(),
                "{ctx}: dot-command '{cmd}' must surface stale-transcript notice"
            );
        }
    }
    assert_eq!(
        recorded.lock().unwrap().len(),
        dispatched_before,
        "{ctx}: dot-command mutations must be rejected while repair is pending"
    );
    assert!(tui.history_command_queue.is_empty());
    assert!(!tui.history_command_running);
}

#[tokio::test]
async fn handoff_rebuild_failure_surfaces_error_never_blanks_retries_paced_and_restores_actionability(
) {
    let _env_lock = crate::test_utils::ENV_LOCK.lock().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::test_utils::TestEnvironment::set(temp.path());
    create_agent_stubs(&temp.path().join("agents"), &["test-agent"]);
    let config = test_config_with_mock_client_and_agent("test-agent", Some("session-parent"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript = vec![user_row(1, "Parent prompt")];
    tui.history_repair_retry_delay_override = Some(Duration::from_millis(15));
    let recorded = record_dispatched_commands(&mut tui);

    let attempts = Arc::new(AtomicUsize::new(0));
    let modes_seen = Arc::new(Mutex::new(Vec::new()));
    let attempts_hook = attempts.clone();
    let modes_hook = modes_seen.clone();
    tui.history_load_override = Some(Arc::new(move |_cfg, mode, _abort| {
        let attempts = attempts_hook.clone();
        let modes = modes_hook.clone();
        Box::pin(async move {
            modes.lock().unwrap().push(mode);
            match attempts.fetch_add(1, Ordering::SeqCst) {
                0 => anyhow::bail!("handoff history unavailable 1"),
                1 => anyhow::bail!("handoff history unavailable 2"),
                _ => Ok(vec![
                    user_row(1, "Handoff child prompt"),
                    assistant_row(2, "Handoff child reply"),
                ]),
            }
        })
    }));

    let handoff_event = handoff_committed_event(&mut tui, "session-child");
    tui.handle_tui_event(handoff_event).await.unwrap();

    // First failure: ReplaceAll handoff must NOT leave a blank view.
    wait_for_attempt_and_idle(&mut tui, &attempts, 1).await;
    assert!(
        !tui.app.transcript.is_empty(),
        "handoff view must never be blank"
    );
    assert!(tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::ErrorText(t) if t.contains("Failed to load session history: handoff history unavailable 1")
    )));
    assert!(tui.is_history_repair_pending());
    assert!(tui.history_repair_retry_handle.is_some());
    assert_row_actions_rejected(&mut tui, &recorded, false, false, "handoff first failure").await;

    // Second failure (paced RepairInitial): updates error in place, no duplicate error row.
    wait_for_attempt_and_idle(&mut tui, &attempts, 2).await;
    let error_rows: Vec<&str> = tui
        .app
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::ErrorText(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        error_rows.len(),
        1,
        "retry error must update in place: {error_rows:?}"
    );
    assert!(error_rows[0].contains("handoff history unavailable 2"));
    assert!(tui.is_history_repair_pending());
    tui.app.llm_busy = false;
    tui.app.pending_message = None;
    assert_row_actions_rejected(&mut tui, &recorded, false, true, "handoff retry failure").await;

    // Third attempt succeeds: error cleared, child rows shown, actionability restored.
    wait_for_repair_completion(&mut tui).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(
        modes_seen.lock().unwrap().as_slice(),
        [
            RebuildMode::ReplaceAll,
            RebuildMode::RepairInitial,
            RebuildMode::RepairInitial
        ]
    );
    assert!(!tui
        .app
        .transcript
        .iter()
        .any(|item| matches!(item, TranscriptItem::ErrorText(_))));
    assert!(tui
        .app
        .transcript
        .iter()
        .any(|item| matches!(item, TranscriptItem::AssistantText { text, .. } if text == "Handoff child reply")));
    assert!(tui.actionable_transcript_binding().is_some());

    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    assert!(matches!(
        tui.app.modal,
        Some(ModalState::ConfirmDelete { from: 2, to: 2, .. })
    ));
    send_key(&mut tui, KeyCode::Char('y'), KeyModifiers::NONE).await;
    assert_eq!(
        recorded.lock().unwrap().as_slice(),
        [".delete message 2"],
        "actionability must restore once handoff repair commits"
    );
}

#[tokio::test]
async fn handoff_failure_with_live_rows_and_empty_recovery_never_blanks_view() {
    let _env_lock = crate::test_utils::ENV_LOCK.lock().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::test_utils::TestEnvironment::set(temp.path());
    create_agent_stubs(&temp.path().join("agents"), &["test-agent"]);
    let config = test_config_with_mock_client_and_agent("test-agent", Some("session-parent"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.history_repair_retry_delay_override = Some(Duration::from_millis(15));

    let first_started = Arc::new(Notify::new());
    let (release_tx, release_rx) = watch::channel(false);
    let attempts = Arc::new(AtomicUsize::new(0));
    let first_started_hook = first_started.clone();
    let attempts_hook = attempts.clone();
    tui.history_load_override = Some(Arc::new(move |_cfg, _mode, abort| {
        let first_started = first_started_hook.clone();
        let mut release_rx = release_rx.clone();
        let attempts = attempts_hook.clone();
        Box::pin(async move {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                first_started.notify_one();
                tokio::select! {
                    _ = harnx_core::abort::wait_abort_signal(&abort) => anyhow::bail!("aborted"),
                    _ = release_rx.wait_for(|v| *v) => {}
                }
                anyhow::bail!("handoff read timed out");
            }
            // Empty child history on repair retry.
            Ok(vec![])
        })
    }));

    let handoff_event = handoff_committed_event(&mut tui, "session-child-empty");
    tui.handle_tui_event(handoff_event).await.unwrap();

    first_started.notified().await;
    // Live row lands on the child transcript before the initial ReplaceAll fails.
    tui.app
        .transcript
        .push(user_row(1, "Live child row before failure"));
    let _ = release_tx.send(true);

    wait_for_attempt_and_idle(&mut tui, &attempts, 1).await;
    assert!(tui
        .app
        .transcript
        .iter()
        .any(|item| matches!(item, TranscriptItem::UserText { text, .. } if text == "Live child row before failure")));
    assert!(tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::ErrorText(t) if t.contains("Failed to load session history: handoff read timed out")
    )));
    assert!(tui.actionable_transcript_binding().is_none());

    // Repair succeeds with empty history -> Welcome wrapper keeps view non-blank and restores binding.
    wait_for_repair_completion(&mut tui).await;
    assert!(
        !tui.app.transcript.is_empty(),
        "empty repaired handoff must still render welcome header"
    );
    assert!(tui.app.transcript.iter().any(
        |item| matches!(item, TranscriptItem::SystemText(t) if t.starts_with("Welcome to harnx"))
    ));
    assert!(!tui
        .app
        .transcript
        .iter()
        .any(|item| matches!(item, TranscriptItem::ErrorText(_))));
    assert!(tui.actionable_transcript_binding().is_some());
}

async fn assert_first_mutation_rebuild_failure(
    tui: &mut Tui,
    attempts: &AtomicUsize,
    recorded: &Mutex<Vec<String>>,
) {
    wait_for_attempt_and_idle(tui, attempts, 1).await;
    assert!(tui.app.transcript.iter().any(
        |item| matches!(item, TranscriptItem::UserText { text, .. } if text == "Old prompt before delete")
    ));
    assert!(tui.app.transcript.iter().any(
        |item| matches!(item, TranscriptItem::MutationNotice(text) if text == "Deleted message 2")
    ));
    assert!(!tui.app.transcript.iter().any(
        |item| matches!(item, TranscriptItem::StatusLine(text) if text == "Loading session history…")
    ));
    assert!(tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::ErrorText(text) if text.contains("broker read failed 1")
    )));
    assert!(tui.is_history_repair_pending());
    assert_row_actions_rejected(tui, recorded, true, true, "post-mutation failure 1").await;
}

async fn assert_retry_mutation_rebuild_failure(
    tui: &mut Tui,
    attempts: &AtomicUsize,
    recorded: &Mutex<Vec<String>>,
) {
    wait_for_attempt_and_idle(tui, attempts, 2).await;
    let stale_errors: Vec<&str> = tui
        .app
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::ErrorText(text) if text.contains("Transcript may be stale") => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(stale_errors.len(), 1, "stale error must update in place");
    assert!(stale_errors[0].contains("broker read failed 2"));
    assert_row_actions_rejected(tui, recorded, true, true, "post-mutation retry failure").await;
}

async fn assert_mutation_rebuild_repaired(
    tui: &mut Tui,
    attempts: &AtomicUsize,
    recorded: &Mutex<Vec<String>>,
) {
    wait_for_repair_completion(tui).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert!(!tui
        .app
        .transcript
        .iter()
        .any(|item| matches!(item, TranscriptItem::ErrorText(_))));
    assert!(!tui.app.transcript.iter().any(
        |item| matches!(item, TranscriptItem::UserText { text, .. } if text == "Old prompt before delete")
    ));
    assert!(tui.app.transcript.iter().any(
        |item| matches!(item, TranscriptItem::UserText { text, .. } if text == "Surviving row after delete")
    ));
    assert!(tui.actionable_transcript_binding().is_some());
    send_key(tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    assert!(matches!(
        tui.app.modal,
        Some(ModalState::ConfirmDelete { from: 1, to: 1, .. })
    ));
    send_key(tui, KeyCode::Char('y'), KeyModifiers::NONE).await;
    assert_eq!(recorded.lock().unwrap().as_slice(), [".delete message 1"]);
}

#[tokio::test]
async fn post_mutation_rebuild_failure_preserves_stale_rows_gates_actions_until_repaired() {
    let config = test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript = vec![
        user_row(1, "Old prompt before delete"),
        assistant_row(2, "Old reply before delete"),
    ];
    tui.history_repair_retry_delay_override = Some(Duration::from_millis(15));
    let recorded = record_dispatched_commands(&mut tui);

    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_hook = attempts.clone();
    tui.history_load_override = Some(Arc::new(move |_cfg, mode, _abort| {
        let attempts = attempts_hook.clone();
        Box::pin(async move {
            match attempts.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    assert_eq!(mode, RebuildMode::PreserveTail);
                    anyhow::bail!("broker read failed 1");
                }
                1 => {
                    assert_eq!(mode, RebuildMode::RepairInitial);
                    anyhow::bail!("broker read failed 2");
                }
                _ => {
                    assert_eq!(mode, RebuildMode::RepairInitial);
                    Ok(vec![user_row(1, "Surviving row after delete")])
                }
            }
        })
    }));

    // Simulate a durable mutation completing through TuiEvent::HistoryCommandFinished
    // (including a queued follow-up action being cleared when stale failure hits).
    tui.retire_transcript_actions_for_pending_mutation();
    tui.handle_tui_event(history_command_finished_event(
        ".delete message 2",
        "Deleted message 2",
        Ok(harnx_runtime::commands::CommandOutcome::Continue),
    ))
    .await
    .unwrap();

    assert_first_mutation_rebuild_failure(&mut tui, &attempts, &recorded).await;

    // A failed non-mutation command between retries must not restore actionability.
    tui.handle_tui_event(history_command_finished_event(
        ".set unknown foo",
        "",
        Err(anyhow::anyhow!("unknown setting")),
    ))
    .await
    .unwrap();
    assert_row_actions_rejected(
        &mut tui,
        &recorded,
        true,
        true,
        "after failed command during pending repair",
    )
    .await;

    assert_retry_mutation_rebuild_failure(&mut tui, &attempts, &recorded).await;
    assert_mutation_rebuild_repaired(&mut tui, &attempts, &recorded).await;
}

#[tokio::test]
async fn compaction_rebuild_failure_preserves_precompaction_rows_gates_actions_and_repairs_without_wrapper(
) {
    let config = test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript = vec![user_row(1, "Pre-compaction row")];
    tui.history_repair_retry_delay_override = Some(Duration::from_millis(15));
    let recorded = record_dispatched_commands(&mut tui);

    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_hook = attempts.clone();
    tui.history_load_override = Some(Arc::new(move |_cfg, mode, _abort| {
        let attempts = attempts_hook.clone();
        Box::pin(async move {
            match attempts.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    assert_eq!(mode, RebuildMode::ReplaceAll);
                    anyhow::bail!("compaction snapshot unavailable");
                }
                _ => {
                    assert_eq!(mode, RebuildMode::RepairInitial);
                    Ok(vec![user_row(2, "Compacted surviving row")])
                }
            }
        })
    }));

    let target = (
        "session-a".to_string(),
        crate::types::Tui::current_session_cluster(&tui),
    );
    tui.session_activity_target = Some(target.clone());
    tui.handle_tui_event(TuiEvent::SessionAgent {
        stamp: crate::event_isolation::EventStamp::live(&tui.live_events),
        historical: false,
        session_id: target.0,
        cluster: target.1,
        event: AgentEvent::Session(SessionEvent::CompactingCompleted {
            compaction_id: None,
            outcome: CompactOutcome::Compacted,
        }),
    })
    .await
    .unwrap();

    wait_for_attempt_and_idle(&mut tui, &attempts, 1).await;
    assert!(tui.app.transcript.iter().any(
        |item| matches!(item, TranscriptItem::UserText { text, .. } if text == "Pre-compaction row")
    ));
    assert!(tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::ErrorText(t) if t.contains("compaction snapshot unavailable")
    )));
    assert_row_actions_rejected(&mut tui, &recorded, true, true, "compaction failure").await;

    wait_for_repair_completion(&mut tui).await;
    assert!(!tui
        .app
        .transcript
        .iter()
        .any(|item| matches!(item, TranscriptItem::SystemText(t) if t.starts_with("Welcome to harnx"))),
        "compaction repair must preserve ReplaceAll non-wrapper semantics"
    );
    assert!(tui
        .app
        .transcript
        .iter()
        .any(|item| matches!(item, TranscriptItem::UserText { text, .. } if text == "Compacted surviving row")));
    assert!(tui.actionable_transcript_binding().is_some());
}

#[tokio::test]
async fn background_shared_refresh_failure_preserves_good_history_and_actionability_without_repair_loop(
) {
    let config = test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript = vec![user_row(1, "Good shared row")];
    let recorded = record_dispatched_commands(&mut tui);

    tui.history_load_override = Some(Arc::new(|_cfg, mode, _abort| {
        Box::pin(async move {
            assert_eq!(mode, RebuildMode::SharedRefresh);
            anyhow::bail!("transient shared refresh error");
        })
    }));

    let stamp = crate::event_isolation::EventStamp::snapshot(&tui.live_events);
    tui.schedule_shared_session_refresh(&stamp);
    tui.flush_history_work_for_test().await;

    assert!(!tui.is_history_repair_pending());
    assert!(tui.history_repair_retry_handle.is_none());
    assert!(!tui
        .app
        .transcript
        .iter()
        .any(|item| matches!(item, TranscriptItem::ErrorText(_))));
    assert!(tui.app.transcript.iter().any(
        |item| matches!(item, TranscriptItem::UserText { text, .. } if text == "Good shared row")
    ));
    assert!(tui.actionable_transcript_binding().is_some());

    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('y'), KeyModifiers::NONE).await;
    assert_eq!(recorded.lock().unwrap().as_slice(), [".delete message 1"]);
}

#[tokio::test]
async fn repair_retry_backoff_is_paced_and_resets_without_tight_loop() {
    let config = test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    assert!(tui.history_repair_retry_delay_override.is_none());
    assert_eq!(tui.history_repair_retry_delay, INITIAL_HISTORY_REPAIR_DELAY);

    let target = tui.current_history_target();
    tui.apply_history_rebuild(new_history_rebuild_completion(
        &tui,
        RebuildMode::PreserveTail,
        RebuildFailurePolicy::PreserveStaleWithErrorAndRepair,
        Err("first failure".to_string()),
    ));
    // Initial failure consumed 1s delay and doubled next delay to 2s.
    assert_eq!(tui.history_repair_retry_delay, Duration::from_secs(2));
    assert!(tui.history_repair_retry_handle.is_some());
    tui.abort_history_repair_retry();

    tui.apply_history_rebuild(new_history_rebuild_completion(
        &tui,
        RebuildMode::RepairInitial,
        RebuildFailurePolicy::KeepRepairOrPreserveSilently,
        Err("second failure".to_string()),
    ));
    // Consumed 2s delay and doubled next delay to 4s.
    assert_eq!(tui.history_repair_retry_delay, Duration::from_secs(4));
    tui.abort_history_repair_retry();

    tui.apply_history_rebuild(crate::types::HistoryRebuildCompletion {
        seq: tui.history_rebuild_seq,
        target,
        attachment_gen: tui.session_attachment_gen,
        refresh_rev: tui.history_refresh_rev,
        mutation_fence: tui.transcript_mutation_seq,
        live_stamp: None,
        preserved_from: 0,
        mode: RebuildMode::RepairInitial,
        failure_policy: RebuildFailurePolicy::KeepRepairOrPreserveSilently,
        result: Ok(vec![user_row(1, "Recovered")]),
    });
    assert_eq!(tui.history_repair_retry_delay, INITIAL_HISTORY_REPAIR_DELAY);
    assert!(!tui.is_history_repair_pending());
    assert!(tui.history_repair_retry_handle.is_none());
}
