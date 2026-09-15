use super::test_config;
use crate::test_utils::TuiTestHarness;
use crate::types::{CancellationAction, Tui};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::Arc;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
}

async fn assert_editor_restored_submission_blocked(tui: &mut Tui, test_char: char) {
    assert!(tui.cancellation_editor_restored());
    tui.handle_key(key(KeyCode::Char(test_char))).await.unwrap();
    assert_eq!(tui.app.input.lines(), &[test_char.to_string()]);
    tui.handle_key(key(KeyCode::Enter)).await.unwrap();
    assert!(tui.app.pending_message.is_none());
    assert_eq!(tui.app.input.lines(), &[test_char.to_string()]);
}

fn assert_acceptance_cleared_guard_and_retained_draft(tui: &Tui, draft: &str) {
    assert!(tui.cancellation.is_none());
    assert!(!tui.has_root_cancellation());
    assert_eq!(tui.app.input.lines(), &[draft]);
    assert!(tui.app.pending_message.is_none());
}

type RecordedActions = Arc<std::sync::Mutex<Vec<(Option<String>, CancellationAction)>>>;

fn recording_cancel_factory() -> (RecordedActions, crate::types::ExitCancelFactory) {
    let actions = Arc::new(std::sync::Mutex::new(Vec::new()));
    let factory_actions = Arc::clone(&actions);
    let factory: crate::types::ExitCancelFactory = Arc::new(move |_, _, _, _, expected, action| {
        factory_actions.lock().unwrap().push((expected, action));
        Box::pin(std::future::pending())
    });
    (actions, factory)
}

fn recovering_cancel_factory() -> (RecordedActions, crate::types::ExitCancelFactory) {
    let actions = Arc::new(std::sync::Mutex::new(Vec::new()));
    let factory_actions = Arc::clone(&actions);
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let factory: crate::types::ExitCancelFactory = Arc::new(move |_, _, _, _, expected, action| {
        factory_actions.lock().unwrap().push((expected, action));
        let step = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            match step {
                0 => anyhow::bail!("request timed out"),
                1 => Ok(harnx_execution_control::CancelReceipt {
                    cancelled: true,
                    disposition: harnx_execution_control::CancelDisposition::Requested,
                    cancellation_id: Some("c-retry".into()),
                    execution_id: Some("recovered-42".into()),
                    requested_at: None,
                    unconfirmed_after_ms: 5_000,
                    abandoned: false,
                }),
                _ => std::future::pending().await,
            }
        })
    });
    (actions, factory)
}

#[tokio::test]
async fn failed_without_execution_id_esc_restores_editing_and_guards_submission() {
    let mut tui = Tui::init(&test_config()).await.expect("init test TUI");
    let (actions, factory) = recording_cancel_factory();
    tui.set_exit_cancel_factory(factory);
    tui.start_cancellation("root".into(), "local".into(), None);
    assert_eq!(
        *actions.lock().unwrap(),
        vec![(None, CancellationAction::Request)]
    );

    tui.cancellation.as_mut().unwrap().phase =
        crate::cancellation::CancellationPhase::Failed("persist timeout".into());
    assert!(tui.cancellation.as_ref().unwrap().execution_id.is_none());

    tui.handle_key(key(KeyCode::Esc)).await.unwrap();
    assert!(tui.app.modal.is_none());
    assert_eq!(
        *actions.lock().unwrap(),
        vec![(None, CancellationAction::Request)]
    );
    assert_editor_restored_submission_blocked(&mut tui, 'h').await;
}

#[tokio::test]
async fn requesting_cancellation_esc_preserves_future_and_processes_receipt() {
    let mut tui = Tui::init(&test_config()).await.expect("init test TUI");
    let (tx, rx) = tokio::sync::oneshot::channel();
    let rx = Arc::new(std::sync::Mutex::new(Some(rx)));
    tui.set_exit_cancel_factory(Arc::new(move |_, _, _, _, _, _| {
        let rx = Arc::clone(&rx);
        Box::pin(async move {
            let rx = rx
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| anyhow::anyhow!("done"))?;
            rx.await.map_err(|e| anyhow::anyhow!("{e}"))
        })
    }));
    tui.start_cancellation("root".into(), "local".into(), None);
    assert!(tui.pending_exit_cancel.is_some());

    tui.handle_key(key(KeyCode::Esc)).await.unwrap();
    assert!(tui.cancellation_editor_restored());
    assert!(tui.pending_exit_cancel.is_some());
    assert_editor_restored_submission_blocked(&mut tui, 'r').await;

    tx.send(harnx_execution_control::CancelReceipt {
        cancelled: true,
        disposition: harnx_execution_control::CancelDisposition::Requested,
        cancellation_id: Some("cancel-1".into()),
        execution_id: Some("exec-1".into()),
        requested_at: None,
        unconfirmed_after_ms: 5_000,
        abandoned: false,
    })
    .unwrap();

    tui.poll_pending_exit_cancel().await;
    assert_acceptance_cleared_guard_and_retained_draft(&tui, "r");
    assert!(tui.pending_exit_cancel.is_none());
    assert!(tui.live_events.is_stopped("exec-1"));
}

#[tokio::test]
async fn unconfirmed_cancellation_esc_preserves_draft_until_acceptance_clears_guard() {
    let mut tui = Tui::init(&test_config()).await.expect("init test TUI");
    let (_, factory) = recording_cancel_factory();
    tui.set_exit_cancel_factory(factory);
    tui.start_cancellation("root".into(), "local".into(), None);
    tui.cancellation.as_mut().unwrap().phase = crate::cancellation::CancellationPhase::Unconfirmed;

    tui.handle_key(key(KeyCode::Esc)).await.unwrap();
    assert!(tui.pending_exit_cancel.is_some());
    assert_editor_restored_submission_blocked(&mut tui, 'w').await;

    // Stage 8 has no Stopping tray or cleanup poller. Requested + cancelled is
    // already durable acceptance, even while physical cleanup remains pending.
    tui.monitor_cancellation(harnx_execution_control::CancelReceipt {
        cancelled: true,
        disposition: harnx_execution_control::CancelDisposition::Requested,
        cancellation_id: Some("cancel-2".into()),
        execution_id: Some("exec-2".into()),
        requested_at: None,
        unconfirmed_after_ms: 5_000,
        abandoned: false,
    });
    assert_acceptance_cleared_guard_and_retained_draft(&tui, "w");
}

#[tokio::test]
async fn abandoning_repeated_esc_is_idempotent() {
    let mut tui = Tui::init(&test_config()).await.expect("init test TUI");
    let (actions, factory) = recording_cancel_factory();
    tui.set_exit_cancel_factory(factory);
    tui.start_cancellation("root".into(), "local".into(), None);
    tui.cancellation.as_mut().unwrap().execution_id = Some("exec-3".into());
    tui.cancellation.as_mut().unwrap().phase = crate::cancellation::CancellationPhase::Unconfirmed;

    tui.handle_key(key(KeyCode::Esc)).await.unwrap();
    let expected = vec![
        (None, CancellationAction::Request),
        (Some("exec-3".into()), CancellationAction::Abandon),
    ];
    assert_eq!(*actions.lock().unwrap(), expected);

    tui.handle_key(key(KeyCode::Esc)).await.unwrap();
    tui.handle_key(key(KeyCode::Esc)).await.unwrap();
    assert_eq!(*actions.lock().unwrap(), expected);
}

#[tokio::test]
async fn abandon_failure_keeps_editor_available_submission_blocked_error_visible() {
    let mut harness = TuiTestHarness::with_size(100, 20).await;
    let tui = harness.tui();
    tui.set_exit_cancel_factory(Arc::new(|_, _, _, _, expected, action| {
        Box::pin(async move {
            match action {
                CancellationAction::Request => Ok(harnx_execution_control::CancelReceipt {
                    cancelled: true,
                    disposition: harnx_execution_control::CancelDisposition::Requested,
                    cancellation_id: Some("c-id".into()),
                    execution_id: expected,
                    requested_at: None,
                    unconfirmed_after_ms: 5_000,
                    abandoned: false,
                }),
                CancellationAction::Abandon => anyhow::bail!("abandon rejected by control plane"),
            }
        })
    }));
    tui.start_cancellation("root".into(), "local".into(), None);
    tui.cancellation.as_mut().unwrap().execution_id = Some("exec-fail".into());
    tui.cancellation.as_mut().unwrap().phase = crate::cancellation::CancellationPhase::Unconfirmed;

    tui.handle_key(key(KeyCode::Esc)).await.unwrap();
    tui.poll_pending_exit_cancel().await;
    assert_editor_restored_submission_blocked(tui, 'd').await;

    harness.render();
    let screen = harness.screen_contents();
    assert!(screen.contains("abandon rejected by control plane"));
    assert!(screen.contains("Draft retained"));
}

#[tokio::test]
async fn unknown_id_ctrl_c_retry_acceptance_clears_guard_without_abandonment() {
    let mut tui = Tui::init(&test_config()).await.expect("init test TUI");
    let (actions, factory) = recovering_cancel_factory();
    tui.set_exit_cancel_factory(factory);
    tui.start_cancellation("root".into(), "local".into(), None);
    tui.poll_pending_exit_cancel().await;

    tui.handle_key(key(KeyCode::Esc)).await.unwrap();
    assert!(tui.cancellation_editor_restored());
    assert_eq!(actions.lock().unwrap().len(), 1);

    tui.handle_key(ctrl('c')).await.unwrap();
    assert_eq!(actions.lock().unwrap().len(), 2);
    assert_eq!(
        actions.lock().unwrap()[1],
        (None, CancellationAction::Request)
    );

    assert!(tui.cancellation_editor_restored());
    assert_editor_restored_submission_blocked(&mut tui, 'd').await;
    tui.poll_pending_exit_cancel().await;
    assert_acceptance_cleared_guard_and_retained_draft(&tui, "d");
    assert!(tui.live_events.is_stopped("recovered-42"));
    tui.handle_key(key(KeyCode::Esc)).await.unwrap();
    assert_eq!(actions.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn child_cancellation_esc_passes_through_unchanged() {
    let mut tui = Tui::init(&test_config()).await.expect("init test TUI");
    tui.start_cancellation(
        "subagent-1".into(),
        "local".into(),
        Some("child-exec-1".into()),
    );
    assert!(!tui.has_root_cancellation());

    tui.handle_key(key(KeyCode::Char('e'))).await.unwrap();
    assert_eq!(tui.app.input.lines(), &["e"]);

    tui.handle_key(key(KeyCode::Esc)).await.unwrap();
    assert!(matches!(
        tui.cancellation.as_ref().map(|tray| &tray.phase),
        Some(crate::cancellation::CancellationPhase::Requesting)
    ));
    assert_eq!(tui.app.input.lines(), &["e"]);
}

#[tokio::test]
async fn paste_is_ignored_while_root_cancellation_active_and_editor_not_restored() {
    let mut tui = Tui::init(&test_config()).await.expect("init test TUI");
    tui.set_input_text("existing draft");
    tui.start_cancellation("root".into(), "local".into(), None);
    assert!(tui.has_root_cancellation());
    assert!(!tui.cancellation_editor_restored());

    tui.handle_paste("dropped paste text".into()).await;
    assert_eq!(tui.app.input.lines(), &["existing draft"]);

    tui.cancellation.as_mut().unwrap().phase = crate::cancellation::CancellationPhase::Unconfirmed;
    tui.handle_paste("dropped again".into()).await;
    assert_eq!(tui.app.input.lines(), &["existing draft"]);
}

#[tokio::test]
async fn paste_is_accepted_once_editor_is_restored() {
    let mut tui = Tui::init(&test_config()).await.expect("init test TUI");
    tui.set_input_text("draft: ");
    tui.start_cancellation("root".into(), "local".into(), None);

    tui.handle_key(key(KeyCode::Esc)).await.unwrap();
    assert!(tui.cancellation_editor_restored());

    tui.handle_paste("accepted text".into()).await;
    assert_eq!(tui.app.input.lines(), &["draft: accepted text"]);
}

#[tokio::test]
async fn compact_cancellation_status_hints_fit_within_80_columns() {
    let tui = Tui::init(&test_config()).await.expect("init test TUI");

    let phases = vec![
        (crate::cancellation::CancellationPhase::Requesting, None),
        (crate::cancellation::CancellationPhase::Abandoning, None),
        (
            crate::cancellation::CancellationPhase::Unconfirmed,
            Some("exec-1".to_string()),
        ),
        (crate::cancellation::CancellationPhase::Unconfirmed, None),
        (
            crate::cancellation::CancellationPhase::Failed("short error".into()),
            Some("exec-1".to_string()),
        ),
        (
            crate::cancellation::CancellationPhase::Failed("short error".into()),
            None,
        ),
    ];

    for (phase, execution_id) in phases {
        let tray = crate::cancellation::CancellationTray {
            phase,
            session_id: "root".into(),
            cluster: "local".into(),
            expected: None,
            execution_id: execution_id.clone(),
            editor_restored: true,
        };
        let msg = format!("  {}", tui.cancellation_message(&tray, true));
        assert!(
            msg.chars().count() <= 80,
            "status message exceeds 80 columns ({} cols): {:?}",
            msg.chars().count(),
            msg
        );

        let visible_slice = if msg.len() > 80 { &msg[..80] } else { &msg };
        if execution_id.is_some()
            && matches!(
                tray.phase,
                crate::cancellation::CancellationPhase::Unconfirmed
                    | crate::cancellation::CancellationPhase::Failed(_)
            )
        {
            assert!(
                visible_slice.contains("Esc: resume anyway"),
                "missing Esc hint in visible 80 cols: {visible_slice:?}"
            );
            assert!(
                visible_slice.contains("Ctrl+C: retry"),
                "missing Ctrl+C hint in visible 80 cols: {visible_slice:?}"
            );
        }
    }
}

#[tokio::test]
async fn full_cancellation_tray_pins_all_current_permutations() {
    let tui = Tui::init(&test_config()).await.expect("init test TUI");

    let cases = vec![
        (
            crate::cancellation::CancellationPhase::Requesting,
            None,
            "Requesting cancellation…  Esc: back to editor  Ctrl+D: exit immediately",
        ),
        (
            crate::cancellation::CancellationPhase::Abandoning,
            None,
            "Resuming with a new execution…  Prior work may still be running.  Esc: back to editor  Ctrl+D: exit",
        ),
        (
            crate::cancellation::CancellationPhase::Unconfirmed,
            Some("exec-1".to_string()),
            "Cancellation unconfirmed. Prior work may still be running.  Ctrl+C: retry  Esc: resume anyway  Ctrl+D: exit",
        ),
        (
            crate::cancellation::CancellationPhase::Unconfirmed,
            None,
            "Cancellation unconfirmed. Prior work may still be running.  Ctrl+C: retry  Esc: back to editor  Ctrl+D: exit",
        ),
        (
            crate::cancellation::CancellationPhase::Failed("test failure".into()),
            Some("exec-1".to_string()),
            "Cancellation request failed: test failure  Prior work may still be running.  Ctrl+C: retry  Esc: resume anyway  Ctrl+D: exit",
        ),
        (
            crate::cancellation::CancellationPhase::Failed("test failure".into()),
            None,
            "Cancellation request failed: test failure  Prior work may still be running.  Ctrl+C: retry  Esc: back to editor  Ctrl+D: exit",
        ),
    ];

    for (phase, execution_id, expected_msg) in cases {
        let is_unknown_id = execution_id.is_none();
        let is_unconfirmed_or_failed = matches!(
            phase,
            crate::cancellation::CancellationPhase::Unconfirmed
                | crate::cancellation::CancellationPhase::Failed(_)
        );
        let is_failed = matches!(phase, crate::cancellation::CancellationPhase::Failed(_));

        let tray = crate::cancellation::CancellationTray {
            phase,
            session_id: "root".into(),
            cluster: "local".into(),
            expected: None,
            execution_id,
            editor_restored: false,
        };
        let msg = tui.cancellation_message(&tray, false);
        assert_eq!(msg, expected_msg);

        if is_unknown_id && is_unconfirmed_or_failed {
            assert!(msg.contains("Esc: back to editor"));
            assert!(!msg.contains("Esc: resume anyway"));
        }
        if is_failed {
            assert!(msg.contains("Prior work may still be running."));
        }
    }
}

#[tokio::test]
async fn retry_keeps_observed_target_and_editor_without_interrupting_g2() {
    let mut tui = Tui::init(&test_config()).await.expect("init test TUI");
    let actions = Arc::new(std::sync::Mutex::new(Vec::new()));
    tui.set_exit_cancel_factory(Arc::new({
        let actions = actions.clone();
        move |_, _, session, cluster, execution, action| {
            actions
                .lock()
                .unwrap()
                .push((session, cluster, execution, action));
            Box::pin(std::future::pending())
        }
    }));
    tui.live_events.select(Some("g1".into()));
    tui.start_observed_cancellation(
        "observed-session".into(),
        "observed-cluster".into(),
        "g1".into(),
    );
    tui.handle_key(key(KeyCode::Esc)).await.unwrap();
    assert!(tui.cancellation_editor_restored());

    let g2_abort = harnx_runtime::utils::create_abort_signal();
    tui.current_prompt_abort = Some(g2_abort.clone());
    tui.live_events.select(Some("g2".into()));
    tui.active_remote_session = Some(("new-session".into(), "new-cluster".into()));
    tui.cancellation.as_mut().unwrap().phase =
        crate::cancellation::CancellationPhase::Failed("lost ack".into());
    tui.handle_key(ctrl('c')).await.unwrap();
    assert!(tui.cancellation_editor_restored());
    assert!(
        !g2_abort.aborted(),
        "old-generation retry cannot abort G2 locally"
    );
    let expected = (
        "observed-session".into(),
        "observed-cluster".into(),
        Some("g1".into()),
        CancellationAction::Request,
    );
    assert_eq!(*actions.lock().unwrap(), vec![expected.clone(), expected]);
    tui.handle_key(ctrl('c')).await.unwrap();
    assert_eq!(
        actions.lock().unwrap().len(),
        2,
        "in-flight request is retained"
    );
    assert_editor_restored_submission_blocked(&mut tui, 'r').await;
}
