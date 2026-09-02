use super::test_config;
use crate::remote_session::classify_exit_worker_state;
use crate::render::exit_body_copy;
use crate::types::{
    ExitCancelFactory, ExitCancelFuture, ExitPhase, ExitWorkerState, ModalState, Tui,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_runtime::config::LOCAL_CLUSTER_KEY;
use harnx_runtime::local_orchestrator::LocalWorkerSupervisor;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

fn ctrl(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
}

fn esc() -> KeyEvent {
    KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
}

async fn prompting_exit_tui() -> Tui {
    let config = test_config();
    let mut tui = Tui::init(&config).await.expect("initialize test TUI");
    tui.app.llm_busy = true;
    tui.active_remote_session = Some(("s".to_string(), LOCAL_CLUSTER_KEY.to_string()));
    tui.app.modal = Some(ModalState::ConfirmExit {
        worker_state: ExitWorkerState::LocalOwnedElsewhere,
        phase: ExitPhase::Prompting,
    });
    tui
}

fn assert_exit_phase(tui: &Tui, expected: ExitPhase) {
    match tui.app.modal.as_ref() {
        Some(ModalState::ConfirmExit { phase, .. }) => assert_eq!(*phase, expected),
        modal => panic!("expected ConfirmExit modal, got {modal:?}"),
    }
}

fn assert_cancel_pending(tui: &Tui) {
    assert_exit_phase(tui, ExitPhase::Interrupting);
    assert!(!tui.app.should_quit);
    assert!(tui.pending_exit_cancel.is_some());
}

async fn assert_interrupt_in_flight(
    tui: &Tui,
    local_worker: &Arc<Mutex<Option<LocalWorkerSupervisor>>>,
    local_worker_was_present: bool,
) {
    assert_cancel_pending(tui);
    assert!(Arc::ptr_eq(local_worker, &tui.local_worker));
    let local_worker_is_present = tui.local_worker.lock().await.is_some();
    assert_eq!(local_worker_is_present, local_worker_was_present);
}

fn assert_exit_finished(tui: &Tui) {
    assert!(tui.app.should_quit);
    assert!(tui.app.modal.is_none());
    assert!(tui.pending_exit_cancel.is_none());
}

fn controlled_cancel_factory(
    release: Arc<Notify>,
    error: Option<&'static str>,
) -> ExitCancelFactory {
    Arc::new(move |_, _, _, _| -> ExitCancelFuture {
        let release = Arc::clone(&release);
        Box::pin(async move {
            release.notified().await;
            match error {
                Some(message) => Err(anyhow::anyhow!(message)),
                None => Ok(()),
            }
        })
    })
}

#[test]
fn exit_worker_classification_detects_remote_session() {
    assert_eq!(
        classify_exit_worker_state("remote", Some("w1"), Ok(Some("w1"))),
        ExitWorkerState::Remote
    );
}

#[test]
fn exit_worker_classification_detects_local_owner() {
    assert_eq!(
        classify_exit_worker_state(LOCAL_CLUSTER_KEY, Some("w1"), Ok(Some("w1"))),
        ExitWorkerState::LocalOwnedHere
    );
}

#[test]
fn exit_worker_classification_detects_local_session_owned_elsewhere() {
    assert_eq!(
        classify_exit_worker_state(LOCAL_CLUSTER_KEY, Some("w1"), Ok(Some("w2"))),
        ExitWorkerState::LocalOwnedElsewhere
    );
    assert_eq!(
        classify_exit_worker_state(LOCAL_CLUSTER_KEY, Some("w1"), Ok(None)),
        ExitWorkerState::LocalOwnedElsewhere
    );
    assert_eq!(
        classify_exit_worker_state(LOCAL_CLUSTER_KEY, None, Ok(Some("w2"))),
        ExitWorkerState::LocalOwnedElsewhere
    );
}

#[test]
fn exit_worker_classification_maps_lease_error_to_unknown() {
    assert_eq!(
        classify_exit_worker_state(LOCAL_CLUSTER_KEY, Some("w1"), Err(())),
        ExitWorkerState::Unknown
    );
}

#[test]
fn exit_body_copy_matches_approved_text() {
    let cases = [
        (
            ExitWorkerState::Remote,
            "Runs on a remote worker. Exit without interrupting and it keeps running there; reopening the session resumes it.",
        ),
        (
            ExitWorkerState::LocalOwnedHere,
            "Runs on a local worker owned by this client. Exit without interrupting and the work stops; reopening the session resumes it from where it stopped.",
        ),
        (
            ExitWorkerState::LocalOwnedElsewhere,
            "Runs on a local worker owned by another client. Exit without interrupting and it keeps running there; reopening the session resumes it.",
        ),
        (
            ExitWorkerState::Unknown,
            "May keep running after you exit. If still in progress when you reopen, it resumes.",
        ),
    ];

    for (state, expected) in cases {
        assert_eq!(exit_body_copy(state), expected);
    }
}

#[tokio::test]
async fn idle_ctrl_d_and_exit_gate_quit_without_modal() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.expect("initialize test TUI");

    tui.handle_key(ctrl('d')).await.expect("handle Ctrl+D");

    assert!(tui.app.should_quit);
    assert!(tui.app.modal.is_none());
    assert!(tui.abort_signal.aborted_ctrld());

    let config = test_config();
    let mut tui = Tui::init(&config).await.expect("initialize test TUI");
    tui.request_exit().await;

    assert!(tui.app.should_quit);
    assert!(tui.app.modal.is_none());
}

#[tokio::test]
async fn idle_picker_exit_quits_without_exit_confirmation() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.expect("initialize test TUI");
    tui.app.modal = Some(ModalState::AgentPicker {
        agents: vec!["test-agent".to_string()],
        selected: 0,
        query: String::new(),
    });

    tui.handle_key(ctrl('d'))
        .await
        .expect("handle picker Ctrl+D");

    assert!(tui.app.should_quit);
    assert!(matches!(
        tui.app.modal,
        Some(ModalState::AgentPicker { .. })
    ));
}

#[tokio::test]
async fn busy_exit_gate_opens_prompting_modal_without_quitting() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.expect("initialize test TUI");
    tui.app.llm_busy = true;
    tui.active_remote_session = Some(("s".to_string(), LOCAL_CLUSTER_KEY.to_string()));

    tui.request_exit().await;

    assert_exit_phase(&tui, ExitPhase::Prompting);
    assert!(!tui.app.should_quit);
}

#[tokio::test]
async fn prompting_ctrl_d_quits_without_starting_cancel() {
    let mut tui = prompting_exit_tui().await;

    tui.handle_modal_key(ctrl('d'))
        .await
        .expect("handle exit Ctrl+D");

    assert_exit_finished(&tui);
    assert!(tui.abort_signal.aborted_ctrld());
    assert!(tui.exit_interrupt_error().is_none());
}

#[tokio::test]
async fn prompting_escape_dismisses_exit_modal_and_stays() {
    let mut tui = prompting_exit_tui().await;

    tui.handle_modal_key(esc())
        .await
        .expect("handle exit Escape");

    assert!(!tui.app.should_quit);
    assert!(tui.app.modal.is_none());
    assert!(tui.pending_exit_cancel.is_none());
}

#[tokio::test]
async fn completed_turn_race_exits_without_starting_cancel() {
    let mut tui = prompting_exit_tui().await;
    tui.app.llm_busy = false;

    tui.handle_modal_key(ctrl('c'))
        .await
        .expect("handle exit Ctrl+C after completion");

    assert_exit_finished(&tui);
    assert!(tui.exit_interrupt_error().is_none());
}

#[tokio::test]
async fn interrupt_exit_waits_for_cancel_before_quitting_and_keeps_worker_owner() {
    let mut tui = prompting_exit_tui().await;
    let release = Arc::new(Notify::new());
    tui.set_exit_cancel_factory(controlled_cancel_factory(Arc::clone(&release), None));
    let local_worker = Arc::clone(&tui.local_worker);
    let local_worker_was_present = tui.local_worker.lock().await.is_some();

    tui.handle_modal_key(ctrl('c'))
        .await
        .expect("start exit interrupt");

    assert_interrupt_in_flight(&tui, &local_worker, local_worker_was_present).await;

    tui.handle_modal_key(esc())
        .await
        .expect("ignore input while interrupting");
    tui.poll_pending_exit_cancel().await;

    assert_interrupt_in_flight(&tui, &local_worker, local_worker_was_present).await;

    release.notify_one();
    tui.poll_pending_exit_cancel().await;

    assert_exit_finished(&tui);
    assert!(tui.exit_interrupt_error().is_none());
}

#[tokio::test]
async fn interrupt_exit_failure_quits_and_records_warning_detail() {
    let mut tui = prompting_exit_tui().await;
    let release = Arc::new(Notify::new());
    tui.set_exit_cancel_factory(controlled_cancel_factory(
        Arc::clone(&release),
        Some("boom"),
    ));

    tui.handle_modal_key(ctrl('c'))
        .await
        .expect("start failing exit interrupt");

    assert_cancel_pending(&tui);

    tui.poll_pending_exit_cancel().await;
    assert_cancel_pending(&tui);

    release.notify_one();
    tui.poll_pending_exit_cancel().await;

    assert_exit_finished(&tui);
    assert_eq!(tui.exit_interrupt_error(), Some("boom"));
}
