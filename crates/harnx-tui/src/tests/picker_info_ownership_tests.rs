//! Regression tests for B2: picker refresh and info-overlay completion ownership.
//!
//! Exercises actual UI/command dispatch (`submit_input_line`, `handle_key`,
//! `handle_ctrl_c`, `handle_tui_event`) with raced completions already queued
//! on `event_rx` before supersession, Ctrl-C cancellation, picker close/reopen,
//! and session/agent switching, verifying stale completions neither clobber UI
//! state nor disarm newer jobs' cancellation slots.

use super::*;
use crate::types::{ModalState, TranscriptItem, TuiEvent};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::{watch, Notify};

fn picker_meta(id: &str) -> harnx_runtime::config::SessionMeta {
    harnx_runtime::config::SessionMeta {
        id: id.to_string(),
        session_id: Some(id.to_string()),
        agent_name: Some("test-agent".to_string()),
        title: None,
        user_id: None,
        modified: None,
        contexts: vec![],
        unread: false,
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

async fn drain_events(tui: &mut Tui) {
    while let Ok(event) = tui.event_rx.try_recv() {
        tui.handle_tui_event(event).await.unwrap();
    }
}

#[derive(Clone, Copy)]
enum QueuedEventKind {
    PickerSessions,
    InfoOverlay,
}

async fn wait_for_queued_event(tui: &mut Tui, kind: QueuedEventKind) -> Vec<TuiEvent> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let mut buffered = Vec::new();
        while let Ok(event) = tui.event_rx.try_recv() {
            let target_queued = match kind {
                QueuedEventKind::PickerSessions => {
                    matches!(event, TuiEvent::PickerSessionsLoaded { .. })
                }
                QueuedEventKind::InfoOverlay => {
                    matches!(event, TuiEvent::InfoOverlayLoaded { .. })
                }
            };
            buffered.push(event);
            if target_queued {
                return buffered;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for queued event"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

async fn open_session_picker_and_select(tui: &mut Tui, target_session: &str) {
    let origin_session = tui
        .config
        .read()
        .session
        .as_ref()
        .map(|s| s.id().to_string());
    tui.app.modal = Some(ModalState::SessionPicker {
        sessions: vec![picker_meta(target_session)],
        selected: 1,
        origin_agent: Some("test-agent".to_string()),
        origin_session,
        error: None,
    });
    send_key(tui, KeyCode::Enter, KeyModifiers::NONE).await;
}

#[tokio::test]
async fn picker_supersession_and_reopen_reject_queued_stale_completion_and_keep_newer_cancellable()
{
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.history_load_override = Some(Arc::new(|_cfg, _mode, _abort| {
        Box::pin(async move { Ok(vec![TranscriptItem::StatusLine("loaded".into())]) })
    }));

    let call_count = Arc::new(AtomicUsize::new(0));
    let (_gate_tx, gate_rx) = watch::channel(false);
    let gated_started = Arc::new(Notify::new());
    let call_count_cb = call_count.clone();
    let gated_started_cb = gated_started.clone();

    tui.picker_sessions_override = Some(Arc::new(move |_cfg, abort| {
        let idx = call_count_cb.fetch_add(1, Ordering::SeqCst);
        let mut gate_rx = gate_rx.clone();
        let gated_started_cb = gated_started_cb.clone();
        Box::pin(async move {
            if idx.is_multiple_of(2) {
                // Even calls (0, 2) finish immediately to race their queued event
                // against subsequent supersession / close+reopen.
                (vec![picker_meta(&format!("stale-{idx}"))], None)
            } else {
                gated_started_cb.notify_one();
                tokio::select! {
                    _ = harnx_core::abort::wait_abort_signal(&abort) => (vec![], Some("aborted".into())),
                    _ = gate_rx.wait_for(|open| *open) => {
                        (vec![picker_meta(&format!("fresh-{idx}"))], None)
                    }
                }
            }
        })
    }));

    // 1. Open picker #1 via UI flow (spawns job 0, which queues PickerSessionsLoaded).
    tui.open_session_picker_with_origin(None, None);
    let queued_job0 = wait_for_queued_event(&mut tui, QueuedEventKind::PickerSessions).await;

    // 2. Supersede job 0 via RefreshSessionList (spawns gated job 1) BEFORE draining job 0.
    tui.handle_tui_event(TuiEvent::RefreshSessionList)
        .await
        .unwrap();
    gated_started.notified().await;
    let job1_abort = tui
        .picker_refresh_abort
        .clone()
        .expect("job 1 must hold abort slot");
    assert!(!job1_abort.aborted_ctrlc());

    // Deliver job 0's already-queued completion through real event dispatch.
    for event in queued_job0 {
        tui.handle_tui_event(event).await.unwrap();
    }
    assert!(
        tui.picker_refresh_abort.is_some(),
        "stale PickerSessionsLoaded must not clear job 1's abort slot"
    );
    assert!(
        !job1_abort.aborted_ctrlc(),
        "job 1 must remain active and un-aborted"
    );
    assert!(
        matches!(
            &tui.app.modal,
            Some(ModalState::SessionPicker { sessions, .. }) if sessions.is_empty()
        ),
        "stale PickerSessionsLoaded must not populate superseded picker"
    );

    // Prove job 1 is still cancellable via Ctrl-C.
    tui.handle_ctrl_c().await;
    assert!(
        job1_abort.aborted_ctrlc(),
        "Ctrl-C after stale completion must still abort job 1"
    );
    assert!(tui.picker_refresh_abort.is_none());
}

#[tokio::test]
async fn reopened_picker_rejects_queued_old_completion_and_accepts_current_result() {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.history_load_override = Some(Arc::new(|_cfg, _mode, _abort| {
        Box::pin(async { Ok(vec![TranscriptItem::StatusLine("loaded".into())]) })
    }));

    let call_count = Arc::new(AtomicUsize::new(0));
    let (gate_tx, gate_rx) = watch::channel(false);
    let gated_started = Arc::new(Notify::new());
    let call_count_hook = call_count.clone();
    let gated_started_hook = gated_started.clone();
    tui.picker_sessions_override = Some(Arc::new(move |_cfg, abort| {
        let idx = call_count_hook.fetch_add(1, Ordering::SeqCst);
        let mut gate_rx = gate_rx.clone();
        let gated_started = gated_started_hook.clone();
        Box::pin(async move {
            if idx == 0 {
                (vec![picker_meta("stale")], None)
            } else {
                gated_started.notify_one();
                tokio::select! {
                    _ = harnx_core::abort::wait_abort_signal(&abort) => (vec![], Some("aborted".into())),
                    _ = gate_rx.wait_for(|open| *open) => (vec![picker_meta("fresh")], None),
                }
            }
        })
    }));

    tui.open_session_picker_with_origin(None, None);
    let queued_old_completion =
        wait_for_queued_event(&mut tui, QueuedEventKind::PickerSessions).await;
    send_key(&mut tui, KeyCode::Esc, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Esc, KeyModifiers::NONE).await;
    assert!(tui.app.modal.is_none());

    submit_input_line(&mut tui, ".session").await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while !matches!(tui.app.modal, Some(ModalState::SessionPicker { .. })) {
        if let Ok(event) = tui.event_rx.try_recv() {
            assert!(
                !matches!(event, TuiEvent::PickerSessionsLoaded { .. }),
                "old picker completion must remain held until the new picker is ready"
            );
            tui.handle_tui_event(event).await.unwrap();
            continue;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    gated_started.notified().await;
    let current_abort = tui
        .picker_refresh_abort
        .clone()
        .expect("reopened picker must own current refresh abort slot");

    for event in queued_old_completion {
        tui.handle_tui_event(event).await.unwrap();
    }
    assert!(tui.picker_refresh_abort.is_some() && !current_abort.aborted_ctrlc());
    assert!(matches!(
        &tui.app.modal,
        Some(ModalState::SessionPicker { sessions, .. }) if sessions.is_empty()
    ));

    let _ = gate_tx.send(true);
    for event in wait_for_queued_event(&mut tui, QueuedEventKind::PickerSessions).await {
        tui.handle_tui_event(event).await.unwrap();
    }
    assert!(tui.picker_refresh_abort.is_none());
    let Some(ModalState::SessionPicker { sessions, .. }) = &tui.app.modal else {
        panic!("expected reopened SessionPicker");
    };
    assert_eq!(
        sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ["fresh"]
    );
}

#[tokio::test]
async fn picker_ctrl_c_and_target_switch_reject_queued_picker_completion() {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_cb = call_count.clone();

    tui.picker_sessions_override = Some(Arc::new(move |_cfg, _abort| {
        let idx = call_count_cb.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { (vec![picker_meta(&format!("listed-{idx}"))], None) })
    }));

    // Case A: Queued completion arrives after Ctrl-C.
    tui.open_session_picker();
    let queued_before_ctrlc =
        wait_for_queued_event(&mut tui, QueuedEventKind::PickerSessions).await;
    tui.handle_ctrl_c().await;
    assert!(tui.picker_refresh_abort.is_none());
    for event in queued_before_ctrlc {
        tui.handle_tui_event(event).await.unwrap();
    }
    assert!(
        matches!(
            &tui.app.modal,
            Some(ModalState::SessionPicker { sessions, .. }) if sessions.is_empty()
        ),
        "Ctrl-C must invalidate already-queued PickerSessionsLoaded"
    );

    // Case B: Queued completion from agent-a arrives after switching to agent-b.
    tui.open_session_picker();
    let queued_agent_a = wait_for_queued_event(&mut tui, QueuedEventKind::PickerSessions).await;
    {
        let mut cfg = tui.config.write();
        cfg.remote_agent = Some(("other-agent".to_string(), "default".to_string()));
    }
    for event in queued_agent_a {
        tui.handle_tui_event(event).await.unwrap();
    }
    assert!(
        matches!(
            &tui.app.modal,
            Some(ModalState::SessionPicker { sessions, .. }) if sessions.is_empty()
        ),
        "target switch must reject queued PickerSessionsLoaded for old agent"
    );
}

async fn info_overlay_tui() -> (Tui, u64) {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.history_load_override = Some(Arc::new(|cfg, _mode, _abort| {
        Box::pin(async move {
            let sid = cfg
                .read()
                .session
                .as_ref()
                .map(|session| session.id().to_string())
                .unwrap_or_default();
            Ok(vec![TranscriptItem::UserText {
                text: format!("row in {sid}"),
                seq: Some(1),
                timestamp: None,
            }])
        })
    }));
    tui.schedule_history_rebuild_for_command(false);
    tui.flush_history_work_for_test().await;
    let initial_revision = tui
        .actionable_transcript_binding()
        .expect("initial session view must be actionable")
        .view_revision;
    (tui, initial_revision)
}

fn install_gated_dump_overlay(tui: &mut Tui) -> (watch::Sender<bool>, Arc<Notify>) {
    let (gate_tx, gate_rx) = watch::channel(false);
    let gated_started = Arc::new(Notify::new());
    let gated_started_hook = gated_started.clone();
    tui.info_overlay_override = Some(Arc::new(move |cfg, _tokens, info_type, abort| {
        let mut gate_rx = gate_rx.clone();
        let gated_started = gated_started_hook.clone();
        Box::pin(async move {
            let sid = cfg
                .read()
                .session
                .as_ref()
                .map(|session| session.id().to_string())
                .unwrap_or_default();
            if matches!(
                info_type,
                crate::input_commands::InfoOverlayType::SessionDump
            ) {
                gated_started.notify_one();
                tokio::select! {
                    _ = harnx_core::abort::wait_abort_signal(&abort) => anyhow::bail!("aborted"),
                    _ = gate_rx.wait_for(|open| *open) => Ok(format!("FRESH DUMP {sid}")),
                }
            } else {
                Ok(format!("STALE INFO {sid}"))
            }
        })
    }));
    (gate_tx, gated_started)
}

#[tokio::test]
async fn superseded_info_completion_cannot_clear_or_replace_newer_dump() {
    let (mut tui, _) = info_overlay_tui().await;
    let (gate_tx, gated_started) = install_gated_dump_overlay(&mut tui);

    submit_input_line(&mut tui, ".info session").await;
    let queued_info = wait_for_queued_event(&mut tui, QueuedEventKind::InfoOverlay).await;
    submit_input_line(&mut tui, ".dump session").await;
    gated_started.notified().await;
    let dump_abort = tui
        .info_overlay_abort
        .clone()
        .expect("superseding dump owns info-overlay abort slot");

    for event in queued_info {
        tui.handle_tui_event(event).await.unwrap();
    }
    assert!(!tui.app.detail_view_open && tui.app.detail_view_text.is_none());
    assert!(tui.info_overlay_abort.is_some() && !dump_abort.aborted_ctrlc());

    tui.handle_ctrl_c().await;
    assert!(dump_abort.aborted_ctrlc());
    assert!(tui.info_overlay_abort.is_none());
    let _ = gate_tx.send(true);
    drain_events(&mut tui).await;
    assert!(!tui.app.detail_view_open);
}

#[tokio::test]
async fn ctrl_c_rejects_info_completion_already_queued() {
    let (mut tui, _) = info_overlay_tui().await;
    tui.info_overlay_override = Some(Arc::new(|_cfg, _tokens, _info_type, _abort| {
        Box::pin(async { Ok("stale info".to_string()) })
    }));

    submit_input_line(&mut tui, ".info session").await;
    let queued_before_ctrlc = wait_for_queued_event(&mut tui, QueuedEventKind::InfoOverlay).await;
    tui.handle_ctrl_c().await;
    for event in queued_before_ctrlc {
        tui.handle_tui_event(event).await.unwrap();
    }
    assert!(!tui.app.detail_view_open && tui.app.detail_view_text.is_none());
}

#[tokio::test]
async fn session_switch_rejects_queued_info_for_previous_session() {
    let (mut tui, _) = info_overlay_tui().await;
    tui.info_overlay_override = Some(Arc::new(|cfg, _tokens, _info_type, _abort| {
        Box::pin(async move {
            let sid = cfg
                .read()
                .session
                .as_ref()
                .map(|session| session.id().to_string())
                .unwrap_or_default();
            Ok(format!("STALE INFO {sid}"))
        })
    }));

    submit_input_line(&mut tui, ".info session").await;
    let queued_from_a = wait_for_queued_event(&mut tui, QueuedEventKind::InfoOverlay).await;
    open_session_picker_and_select(&mut tui, "session-b").await;
    tui.flush_history_work_for_test().await;
    for event in queued_from_a {
        tui.handle_tui_event(event).await.unwrap();
    }
    assert!(!tui.app.detail_view_open && tui.app.detail_view_text.is_none());
    let b_binding = tui
        .actionable_transcript_binding()
        .expect("session B view remains actionable");
    assert_eq!(b_binding.target.session_id.as_deref(), Some("session-b"));
}

#[tokio::test]
async fn rapid_session_reentry_rejects_info_from_previous_attachment() {
    let (mut tui, _) = info_overlay_tui().await;
    open_session_picker_and_select(&mut tui, "session-b").await;
    tui.flush_history_work_for_test().await;
    let first_b_revision = tui
        .actionable_transcript_binding()
        .expect("first session B view is actionable")
        .view_revision;
    tui.info_overlay_override = Some(Arc::new(|_cfg, _tokens, _info_type, _abort| {
        Box::pin(async { Ok("old session B info".to_string()) })
    }));

    submit_input_line(&mut tui, ".info session").await;
    let queued_from_old_b = wait_for_queued_event(&mut tui, QueuedEventKind::InfoOverlay).await;
    open_session_picker_and_select(&mut tui, "session-a").await;
    tui.flush_history_work_for_test().await;
    open_session_picker_and_select(&mut tui, "session-b").await;
    tui.flush_history_work_for_test().await;
    for event in queued_from_old_b {
        tui.handle_tui_event(event).await.unwrap();
    }
    assert!(!tui.app.detail_view_open && tui.app.detail_view_text.is_none());
    let current_binding = tui
        .actionable_transcript_binding()
        .expect("re-entered session B has current binding");
    assert_eq!(
        current_binding.target.session_id.as_deref(),
        Some("session-b")
    );
    assert!(current_binding.view_revision > first_b_revision);
}
