//! Fencing, ordering, and cancellation tests for T3 history repair and shared
//! session refreshes (designs 6 and 10).

use super::history_repair_tests::{
    attach_target_for_test, has_user_text, init_tui_with_failed_initial_history,
    session_agent_event, stream_attached_event, wait_for_history_rebuild_slot_idle,
};
use super::*;
use crate::history_work::RebuildMode;
use crate::types::HistoryRebuildCompletion;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

fn user_row(seq: usize, text: &str) -> TranscriptItem {
    TranscriptItem::UserText {
        text: text.to_string(),
        seq: Some(seq),
        timestamp: None,
    }
}

fn completion_for(
    tui: &Tui,
    mode: RebuildMode,
    live_stamp: Option<crate::event_isolation::EventStamp>,
    text: &str,
) -> HistoryRebuildCompletion {
    HistoryRebuildCompletion {
        seq: tui.history_rebuild_seq,
        target: tui.current_history_target(),
        attachment_gen: tui.session_attachment_gen,
        refresh_rev: tui.history_refresh_rev,
        mutation_fence: tui.transcript_mutation_seq,
        live_stamp,
        preserved_from: 0,
        mode,
        failure_policy: crate::history_work::RebuildFailurePolicy::KeepRepairOrPreserveSilently,
        result: Ok(vec![user_row(1, text)]),
    }
}

fn install_two_stage_repair_hook(
    tui: &mut Tui,
    first_started: Arc<Notify>,
    release_first: Arc<Notify>,
    second_started: Arc<Notify>,
    release_second: Arc<Notify>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    tui.history_load_override = Some(Arc::new(move |_cfg, _mode, abort| {
        let first_started = first_started.clone();
        let release_first = release_first.clone();
        let second_started = second_started.clone();
        let release_second = release_second.clone();
        let calls = calls.clone();
        Box::pin(async move {
            match calls.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    first_started.notify_one();
                    tokio::select! {
                        _ = harnx_core::abort::wait_abort_signal(&abort) => anyhow::bail!("aborted"),
                        _ = release_first.notified() => {}
                    }
                    Ok(vec![user_row(1, "stale gen-1 row")])
                }
                1 => {
                    second_started.notify_one();
                    tokio::select! {
                        _ = harnx_core::abort::wait_abort_signal(&abort) => anyhow::bail!("aborted"),
                        _ = release_second.notified() => {}
                    }
                    Ok(vec![user_row(2, "stale gen-2 row")])
                }
                _ => Ok(vec![user_row(3, "fresh gen-3 row")]),
            }
        })
    }));
}

#[tokio::test]
async fn same_session_reattach_and_detach_reattach_fence_stale_repairs() {
    let (config, mut tui) = init_tui_with_failed_initial_history("reattach-fence").await;
    let first_started = Arc::new(Notify::new());
    let release_first = Arc::new(Notify::new());
    let second_started = Arc::new(Notify::new());
    let release_second = Arc::new(Notify::new());
    install_two_stage_repair_hook(
        &mut tui,
        first_started.clone(),
        release_first.clone(),
        second_started.clone(),
        release_second.clone(),
    );

    let target = attach_target_for_test(&mut tui);
    tui.handle_tui_event(stream_attached_event(&tui, &target, false))
        .await
        .unwrap();
    first_started.notified().await;
    let (gen1, seq1, rev1) = (
        tui.session_attachment_gen,
        tui.history_rebuild_seq,
        tui.history_refresh_rev,
    );
    let first_abort = tui.history_rebuild_abort.clone().unwrap();

    tui.handle_tui_event(stream_attached_event(&tui, &target, false))
        .await
        .unwrap();
    second_started.notified().await;
    let gen2 = tui.session_attachment_gen;
    assert_eq!(gen2, gen1 + 1);
    assert!(first_abort.aborted_ctrlc());

    let stale_seq = HistoryRebuildCompletion {
        seq: seq1,
        attachment_gen: gen1,
        refresh_rev: rev1,
        ..completion_for(&tui, RebuildMode::RepairInitial, None, "stale gen-1 row")
    };
    tui.apply_history_rebuild(stale_seq);
    assert!(tui.history_rebuild_abort.is_some());

    tui.session_attachment_gen = gen2 + 1;
    tui.history_repair_retry_delay_override = Some(std::time::Duration::from_secs(60));
    let stale_gen = HistoryRebuildCompletion {
        attachment_gen: gen2,
        ..completion_for(&tui, RebuildMode::RepairInitial, None, "stale gen-2 row")
    };
    tui.apply_history_rebuild(stale_gen);
    assert!(tui.history_rebuild_abort.is_none());
    assert!(tui.is_history_repair_pending());
    tui.abort_history_repair_retry();

    tui.schedule_history_repair();
    release_first.notify_waiters();
    release_second.notify_waiters();
    tui.flush_history_work_for_test().await;
    assert!(!tui.is_history_repair_pending());
    assert!(has_user_text(&tui, "fresh gen-3 row"));
    assert!(!has_user_text(&tui, "stale gen-1 row"));
    assert!(!has_user_text(&tui, "stale gen-2 row"));

    let gen_before_detach = tui.session_attachment_gen;
    {
        let mut guard = config.write();
        guard.session =
            Some(harnx_runtime::config::session::new(&guard, "other-session", None).unwrap());
    }
    tui.sync_session_activity_monitor();
    assert!(tui.session_attachment_gen > gen_before_detach);
    assert!(!tui.session_stream_attached);
    tui.stop_session_activity_monitor();
}

#[tokio::test]
async fn live_transcript_mutation_during_repair_discards_snapshot_and_keeps_repair_pending() {
    let (_config, mut tui) = init_tui_with_failed_initial_history("live-mutation").await;
    tui.history_repair_retry_delay_override = Some(std::time::Duration::from_millis(15));
    let first_started = Arc::new(Notify::new());
    let release_first = Arc::new(Notify::new());
    let attempts = Arc::new(AtomicUsize::new(0));
    let (fs_h, rf_h, att_h) = (
        first_started.clone(),
        release_first.clone(),
        attempts.clone(),
    );
    tui.history_load_override = Some(Arc::new(move |_cfg, _mode, _abort| {
        let (first_started, release_first, attempts) = (fs_h.clone(), rf_h.clone(), att_h.clone());
        Box::pin(async move {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                first_started.notify_one();
                release_first.notified().await;
                Ok(vec![user_row(1, "old snapshot before live row")])
            } else {
                Ok(vec![
                    user_row(1, "old snapshot before live row"),
                    TranscriptItem::AssistantText {
                        text: "live streamed row".to_string(),
                        seq: Some(2),
                        timestamp: None,
                        rendered_cache: None,
                    },
                ])
            }
        })
    }));

    let target = attach_target_for_test(&mut tui);
    tui.handle_tui_event(stream_attached_event(&tui, &target, false))
        .await
        .unwrap();
    first_started.notified().await;

    tui.handle_tui_event(TuiEvent::SessionAgent {
        stamp: crate::event_isolation::EventStamp::live(&tui.live_events),
        historical: false,
        session_id: target.0.clone(),
        cluster: target.1.clone(),
        event: harnx_core::event::AgentEvent::Model(harnx_core::event::ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("live streamed row".to_string())],
        }),
    })
    .await
    .unwrap();

    release_first.notify_waiters();
    wait_for_history_rebuild_slot_idle(&mut tui).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(tui.is_history_repair_pending());
    assert!(tui.app.transcript.iter().any(|item| {
        matches!(item, TranscriptItem::AssistantText { text, .. } if text == "live streamed row")
    }));

    wait_for_repair_completion(&mut tui).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(!tui.is_history_repair_pending());
    assert!(tui.app.transcript.iter().any(|item| {
        matches!(item, TranscriptItem::AssistantText { text, .. } if text == "live streamed row")
    }));
}

#[tokio::test]
async fn shared_refresh_shares_serialization_and_prompt_fences_with_rebuild_and_compaction() {
    let (_config, mut tui) = init_tui_with_failed_initial_history("shared-ordering").await;
    tui.history_repair_target = None;
    let refresh_started = Arc::new(Notify::new());
    let release_refresh = Arc::new(Notify::new());
    let (rs_h, rr_h) = (refresh_started.clone(), release_refresh.clone());
    tui.history_load_override = Some(Arc::new(move |_cfg, mode, abort| {
        let (refresh_started, release_refresh) = (rs_h.clone(), rr_h.clone());
        Box::pin(async move {
            if matches!(mode, RebuildMode::SharedRefresh) {
                refresh_started.notify_one();
                tokio::select! {
                    _ = harnx_core::abort::wait_abort_signal(&abort) => anyhow::bail!("aborted"),
                    _ = release_refresh.notified() => {}
                }
                Ok(vec![user_row(1, "stale turn refresh")])
            } else {
                Ok(vec![user_row(2, "compacted replacement")])
            }
        })
    }));

    let _target = attach_target_for_test(&mut tui);
    let stamp = crate::event_isolation::EventStamp::live(&tui.live_events);
    tui.schedule_shared_session_refresh(&stamp);
    refresh_started.notified().await;
    let refresh_abort = tui.history_rebuild_abort.clone().unwrap();

    tui.schedule_history_rebuild_with_mode(RebuildMode::ReplaceAll);
    assert!(refresh_abort.aborted_ctrlc());
    release_refresh.notify_waiters();
    tui.flush_history_work_for_test().await;
    assert!(has_user_text(&tui, "compacted replacement"));
    assert!(!has_user_text(&tui, "stale turn refresh"));

    tui.current_prompt_abort = Some(harnx_runtime::utils::create_abort_signal());
    let blocked_prompt = completion_for(
        &tui,
        RebuildMode::SharedRefresh,
        Some(stamp.clone()),
        "blocked by active prompt",
    );
    tui.apply_history_rebuild(blocked_prompt);
    assert!(!has_user_text(&tui, "blocked by active prompt"));

    tui.current_prompt_abort = None;
    tui.live_events = tui.live_events.fork();
    let blocked_fork = completion_for(
        &tui,
        RebuildMode::SharedRefresh,
        Some(stamp),
        "blocked by live_events fork",
    );
    tui.apply_history_rebuild(blocked_fork);
    assert!(!has_user_text(&tui, "blocked by live_events fork"));
}

#[tokio::test]
async fn target_switch_and_ctrl_c_abort_inflight_repair_and_retry_timer() {
    let (config, mut tui) = init_tui_with_failed_initial_history("session-a").await;
    tui.history_repair_retry_delay_override = Some(std::time::Duration::from_secs(60));
    let started = Arc::new(Notify::new());
    let started_h = started.clone();
    tui.history_load_override = Some(Arc::new(move |_cfg, _mode, abort| {
        let started = started_h.clone();
        Box::pin(async move {
            started.notify_one();
            harnx_core::abort::wait_abort_signal(&abort).await;
            anyhow::bail!("cancelled")
        })
    }));

    let target = attach_target_for_test(&mut tui);
    tui.handle_tui_event(stream_attached_event(&tui, &target, false))
        .await
        .unwrap();
    started.notified().await;
    let first_abort = tui.history_rebuild_abort.clone().unwrap();

    tui.handle_ctrl_c().await;
    assert!(first_abort.aborted_ctrlc());
    assert!(tui.history_rebuild_abort.is_none());

    tui.schedule_history_repair_retry();
    assert!(tui.history_repair_retry_handle.is_some());
    {
        let mut guard = config.write();
        guard.session =
            Some(harnx_runtime::config::session::new(&guard, "session-b", None).unwrap());
    }
    tui.schedule_history_rebuild_for_command(true);
    assert!(tui.history_repair_retry_handle.is_none());
    assert!(!tui.is_history_repair_pending());

    tui.cancel_background_history_work();
    tui.flush_history_work_for_test().await;
}

#[tokio::test]
async fn shared_turn_started_and_ended_through_tui_event_fence_real_transcript_mutations() {
    use harnx_core::event::{AgentEvent, TurnEvent};

    let (_config, mut tui) = init_tui_with_failed_initial_history("turn-event-path").await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_h = attempts.clone();
    tui.history_load_override = Some(Arc::new(move |_cfg, mode, _abort| {
        let attempts = attempts_h.clone();
        Box::pin(async move {
            assert!(matches!(mode, RebuildMode::SharedRefresh));
            match attempts.fetch_add(1, Ordering::SeqCst) {
                0 => Ok(vec![user_row(1, "started snapshot row")]),
                _ => Ok(vec![user_row(2, "ended snapshot row")]),
            }
        })
    }));

    let target = attach_target_for_test(&mut tui);
    let mut_seq_before_start = tui.transcript_mutation_seq;
    tui.handle_tui_event(session_agent_event(
        &tui,
        &target,
        AgentEvent::Turn(TurnEvent::Started),
    ))
    .await
    .unwrap();
    assert!(tui.app.llm_busy);
    assert_eq!(tui.transcript_mutation_seq, mut_seq_before_start);

    tui.flush_history_work_for_test().await;
    assert!(!tui.is_history_repair_pending());
    assert!(has_user_text(&tui, "started snapshot row"));

    let mut_seq_before_end = tui.transcript_mutation_seq;
    tui.handle_tui_event(session_agent_event(
        &tui,
        &target,
        AgentEvent::Turn(TurnEvent::Ended {
            outcome: Default::default(),
        }),
    ))
    .await
    .unwrap();
    assert!(!tui.app.llm_busy);
    assert_eq!(tui.transcript_mutation_seq, mut_seq_before_end);

    tui.flush_history_work_for_test().await;
    assert!(has_user_text(&tui, "ended snapshot row"));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn turn_ended_and_interrupted_flush_pending_thought_and_fence_inflight_rebuild() {
    use harnx_core::event::{AgentEvent, ModelEvent, TurnEvent};

    let (_config, mut tui) = init_tui_with_failed_initial_history("turn-thought-flush").await;
    tui.history_repair_retry_delay_override = Some(std::time::Duration::from_millis(15));
    let first_started = Arc::new(Notify::new());
    let release_first = Arc::new(Notify::new());
    let attempts = Arc::new(AtomicUsize::new(0));
    let (fs_h, rf_h, att_h) = (
        first_started.clone(),
        release_first.clone(),
        attempts.clone(),
    );
    tui.history_load_override = Some(Arc::new(move |_cfg, _mode, _abort| {
        let (first_started, release_first, attempts) = (fs_h.clone(), rf_h.clone(), att_h.clone());
        Box::pin(async move {
            match attempts.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    first_started.notify_one();
                    release_first.notified().await;
                    Ok(vec![user_row(1, "stale pre-thought snapshot")])
                }
                _ => Ok(vec![user_row(2, "post-turn snapshot")]),
            }
        })
    }));

    let target = attach_target_for_test(&mut tui);
    tui.handle_tui_event(session_agent_event(
        &tui,
        &target,
        AgentEvent::Model(ModelEvent::ThoughtChunk {
            blocks: vec![ContentBlock::Text("buffered thought".to_string())],
        }),
    ))
    .await
    .unwrap();

    tui.handle_tui_event(stream_attached_event(&tui, &target, false))
        .await
        .unwrap();
    first_started.notified().await;

    let mut_seq_before_interrupt = tui.transcript_mutation_seq;
    tui.handle_tui_event(TuiEvent::LocalAgent(AgentEvent::Turn(
        TurnEvent::Interrupted {
            cancellation_id: "cancel-1".to_string(),
        },
    )))
    .await
    .unwrap();
    assert_eq!(tui.transcript_mutation_seq, mut_seq_before_interrupt + 1);
    assert!(tui.app.transcript.iter().any(|item| {
        matches!(item, TranscriptItem::ThoughtText(text) if text == "buffered thought")
    }));

    release_first.notify_waiters();
    wait_for_history_rebuild_slot_idle(&mut tui).await;
    assert!(tui.is_history_repair_pending());
    assert!(!has_user_text(&tui, "stale pre-thought snapshot"));

    wait_for_repair_completion(&mut tui).await;
    assert!(!tui.is_history_repair_pending());

    tui.handle_tui_event(session_agent_event(
        &tui,
        &target,
        AgentEvent::Model(ModelEvent::ThoughtChunk {
            blocks: vec![ContentBlock::Text("second thought".to_string())],
        }),
    ))
    .await
    .unwrap();
    let mut_seq_before_ended = tui.transcript_mutation_seq;
    tui.handle_tui_event(session_agent_event(
        &tui,
        &target,
        AgentEvent::Turn(TurnEvent::Ended {
            outcome: Default::default(),
        }),
    ))
    .await
    .unwrap();
    assert_eq!(tui.transcript_mutation_seq, mut_seq_before_ended + 1);
    assert!(tui.app.transcript.iter().any(|item| {
        matches!(item, TranscriptItem::ThoughtText(text) if text == "second thought")
    }));
    tui.flush_history_work_for_test().await;
    assert!(has_user_text(&tui, "post-turn snapshot"));
}

#[tokio::test]
async fn same_session_stream_reattach_fences_inflight_shared_refresh_with_unchanged_seq_and_rev() {
    use harnx_core::event::{AgentEvent, TurnEvent};

    let (_config, mut tui) = init_tui_with_failed_initial_history("reconnect-fences-refresh").await;
    tui.history_repair_target = None;
    let refresh_started = Arc::new(Notify::new());
    let release_refresh = Arc::new(Notify::new());
    let (rs_h, rr_h) = (refresh_started.clone(), release_refresh.clone());
    tui.history_load_override = Some(Arc::new(move |_cfg, mode, _abort| {
        let (refresh_started, release_refresh) = (rs_h.clone(), rr_h.clone());
        Box::pin(async move {
            assert!(matches!(mode, RebuildMode::SharedRefresh));
            refresh_started.notify_one();
            release_refresh.notified().await;
            Ok(vec![user_row(1, "stale pre-reconnect refresh")])
        })
    }));

    let target = attach_target_for_test(&mut tui);
    tui.handle_tui_event(stream_attached_event(&tui, &target, false))
        .await
        .unwrap();
    assert!(tui.session_stream_attached);

    tui.handle_tui_event(session_agent_event(
        &tui,
        &target,
        AgentEvent::Turn(TurnEvent::Started),
    ))
    .await
    .unwrap();
    refresh_started.notified().await;
    let (gen_before, seq_before, rev_before) = (
        tui.session_attachment_gen,
        tui.history_rebuild_seq,
        tui.history_refresh_rev,
    );

    tui.handle_tui_event(stream_attached_event(&tui, &target, false))
        .await
        .unwrap();
    assert_eq!(tui.session_attachment_gen, gen_before + 1);
    assert_eq!(tui.history_rebuild_seq, seq_before);
    assert_eq!(tui.history_refresh_rev, rev_before);

    release_refresh.notify_waiters();
    tui.flush_history_work_for_test().await;
    assert!(!has_user_text(&tui, "stale pre-reconnect refresh"));
}
