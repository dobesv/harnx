use super::test_config;
use crate::test_utils::TuiTestHarness;
use crate::types::Tui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_runtime::nats_session::InterruptOutcome;
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

type RecordedTargets = Arc<std::sync::Mutex<Vec<(String, String)>>>;

fn recording_cancel_factory() -> (RecordedTargets, crate::types::ExitCancelFactory) {
    let targets = Arc::new(std::sync::Mutex::new(Vec::new()));
    let factory_targets = Arc::clone(&targets);
    let factory: crate::types::ExitCancelFactory = Arc::new(move |_, _, session_id, cluster| {
        factory_targets.lock().unwrap().push((session_id, cluster));
        Box::pin(std::future::pending())
    });
    (targets, factory)
}

/// First attempt fails; every attempt after that is durably accepted.
fn recovering_cancel_factory() -> (RecordedTargets, crate::types::ExitCancelFactory) {
    let targets = Arc::new(std::sync::Mutex::new(Vec::new()));
    let factory_targets = Arc::clone(&targets);
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let factory: crate::types::ExitCancelFactory = Arc::new(move |_, _, session_id, cluster| {
        factory_targets.lock().unwrap().push((session_id, cluster));
        let step = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            match step {
                0 => anyhow::bail!("request timed out"),
                _ => Ok(InterruptOutcome::Accepted { cancel_seq: 42 }),
            }
        })
    });
    (targets, factory)
}

#[tokio::test]
async fn failed_esc_restores_editing_and_guards_submission() {
    let mut tui = Tui::init(&test_config()).await.expect("init test TUI");
    tui.active_remote_session = Some(("root".into(), "local".into()));
    let (targets, factory) = recording_cancel_factory();
    tui.set_exit_cancel_factory(factory);
    tui.start_cancellation("root".into(), "local".into());
    assert_eq!(
        *targets.lock().unwrap(),
        vec![("root".to_string(), "local".to_string())]
    );

    tui.cancellation.as_mut().unwrap().phase =
        crate::cancellation::CancellationPhase::Failed("persist timeout".into());

    tui.handle_key(key(KeyCode::Esc)).await.unwrap();
    assert!(tui.app.modal.is_none());
    assert_eq!(
        *targets.lock().unwrap(),
        vec![("root".to_string(), "local".to_string())]
    );
    assert_editor_restored_submission_blocked(&mut tui, 'h').await;
}

#[tokio::test]
async fn requesting_cancellation_esc_preserves_future_and_processes_outcome() {
    let mut tui = Tui::init(&test_config()).await.expect("init test TUI");
    tui.active_remote_session = Some(("root".into(), "local".into()));
    let (tx, rx) = tokio::sync::oneshot::channel();
    let rx = Arc::new(std::sync::Mutex::new(Some(rx)));
    tui.set_exit_cancel_factory(Arc::new(move |_, _, _, _| {
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
    tui.start_cancellation("root".into(), "local".into());
    assert!(tui.pending_exit_cancel.is_some());

    tui.handle_key(key(KeyCode::Esc)).await.unwrap();
    assert!(tui.cancellation_editor_restored());
    assert!(tui.pending_exit_cancel.is_some());
    assert_editor_restored_submission_blocked(&mut tui, 'r').await;

    tx.send(InterruptOutcome::Accepted { cancel_seq: 1 })
        .unwrap();

    tui.poll_pending_exit_cancel().await;
    assert_acceptance_cleared_guard_and_retained_draft(&tui, "r");
    assert!(tui.pending_exit_cancel.is_none());
}

#[tokio::test]
async fn failed_ctrl_c_retry_then_acceptance_clears_guard() {
    let mut tui = Tui::init(&test_config()).await.expect("init test TUI");
    tui.active_remote_session = Some(("root".into(), "local".into()));
    let (targets, factory) = recovering_cancel_factory();
    tui.set_exit_cancel_factory(factory);
    tui.start_cancellation("root".into(), "local".into());
    tui.poll_pending_exit_cancel().await;
    assert!(matches!(
        tui.cancellation.as_ref().map(|tray| &tray.phase),
        Some(crate::cancellation::CancellationPhase::Failed(_))
    ));

    tui.handle_key(key(KeyCode::Esc)).await.unwrap();
    assert!(tui.cancellation_editor_restored());
    assert_eq!(targets.lock().unwrap().len(), 1);

    tui.handle_key(ctrl('c')).await.unwrap();
    assert_eq!(targets.lock().unwrap().len(), 2);
    assert_eq!(
        targets.lock().unwrap()[1],
        ("root".to_string(), "local".to_string())
    );

    // A retry preserves the editor-restored flag set before it.
    assert!(tui.cancellation_editor_restored());
    assert_editor_restored_submission_blocked(&mut tui, 'd').await;
    tui.poll_pending_exit_cancel().await;
    assert_acceptance_cleared_guard_and_retained_draft(&tui, "d");

    // No cancellation is left to react to a further Esc.
    tui.handle_key(key(KeyCode::Esc)).await.unwrap();
    assert_eq!(targets.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn paste_is_ignored_while_root_cancellation_active_and_editor_not_restored() {
    let mut tui = Tui::init(&test_config()).await.expect("init test TUI");
    tui.active_remote_session = Some(("root".into(), "local".into()));
    tui.set_input_text("existing draft");
    tui.start_cancellation("root".into(), "local".into());
    assert!(tui.has_root_cancellation());
    assert!(!tui.cancellation_editor_restored());

    tui.handle_paste("dropped paste text".into()).await;
    assert_eq!(tui.app.input.lines(), &["existing draft"]);

    tui.cancellation.as_mut().unwrap().phase =
        crate::cancellation::CancellationPhase::Failed("boom".into());
    tui.handle_paste("dropped again".into()).await;
    assert_eq!(tui.app.input.lines(), &["existing draft"]);
}

#[tokio::test]
async fn paste_is_accepted_once_editor_is_restored() {
    let mut tui = Tui::init(&test_config()).await.expect("init test TUI");
    tui.active_remote_session = Some(("root".into(), "local".into()));
    tui.set_input_text("draft: ");
    tui.start_cancellation("root".into(), "local".into());

    tui.handle_key(key(KeyCode::Esc)).await.unwrap();
    assert!(tui.cancellation_editor_restored());

    tui.handle_paste("accepted text".into()).await;
    assert_eq!(tui.app.input.lines(), &["draft: accepted text"]);
}

#[tokio::test]
async fn compact_cancellation_status_hints_fit_within_80_columns() {
    let tui = Tui::init(&test_config()).await.expect("init test TUI");

    let phases = vec![
        crate::cancellation::CancellationPhase::Requesting,
        crate::cancellation::CancellationPhase::Failed("short error".into()),
        // A pathologically long error must not push the retry hint, the
        // only thing the user can act on, off the single-line status area.
        crate::cancellation::CancellationPhase::Failed("x".repeat(200)),
    ];

    for phase in phases {
        let is_failed = matches!(phase, crate::cancellation::CancellationPhase::Failed(_));
        let tray = crate::cancellation::CancellationTray {
            phase,
            session_id: "root".into(),
            cluster: "local".into(),
            editor_restored: true,
        };
        let msg = format!("  {}", tui.cancellation_message(&tray, true));
        assert!(
            msg.chars().count() <= 80,
            "status message exceeds 80 columns ({} cols): {:?}",
            msg.chars().count(),
            msg
        );
        if is_failed {
            assert!(
                msg.contains("Ctrl+C: retry"),
                "retry hint must stay within the 80-column budget: {msg:?}"
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
            "Interrupting…  Esc: back to editor  Ctrl+D: exit immediately",
        ),
        (
            crate::cancellation::CancellationPhase::Failed("boom".into()),
            "Interrupt failed: boom  Ctrl+C: retry  Ctrl+D: exit anyway  Esc: back",
        ),
    ];

    for (phase, expected_msg) in cases {
        let tray = crate::cancellation::CancellationTray {
            phase,
            session_id: "root".into(),
            cluster: "local".into(),
            editor_restored: false,
        };
        let msg = tui.cancellation_message(&tray, false);
        assert_eq!(msg, expected_msg);
    }
}

#[tokio::test]
async fn accepted_interrupt_returns_to_editor_with_draft_intact() {
    let mut harness = TuiTestHarness::new().await;
    let tui = harness.tui();
    tui.set_input_text("keep me");
    tui.app.llm_busy = true;
    tui.active_remote_session = Some(("root".into(), "local".into()));
    tui.current_prompt_abort = Some(harnx_runtime::utils::create_abort_signal());
    tui.current_prompt_handle = Some(tokio::spawn(std::future::pending()));
    tui.set_exit_cancel_factory(Arc::new(|_, _, _, _| {
        Box::pin(async { Ok(InterruptOutcome::Accepted { cancel_seq: 7 }) })
    }));

    tui.handle_key(ctrl('c')).await.unwrap();
    tui.poll_pending_exit_cancel().await;

    assert_eq!(tui.app.input.lines(), &["keep me"]);
    assert!(!tui.app.llm_busy);
}
