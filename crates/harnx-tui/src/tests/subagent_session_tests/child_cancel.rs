use super::*;
use crate::types::{MonitoredSessionState, SubAgentInvocationProgress, Tui};
use std::sync::{Arc, Mutex};

type Targets = Arc<Mutex<Vec<(String, Option<String>)>>>;

async fn child_tui() -> (Tui, MonitoredSessionKey, Targets) {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    let key = monitored_key("worker", "selected-child");
    let progress = SubAgentInvocationProgress::new(subagent_progress(
        &key,
        "active",
        SubAgentProgressStatus::Running,
        0,
    ));
    tui.app.transcript = vec![TranscriptItem::SubAgentSession {
        key: key.clone(),
        status: SubAgentStatus::Running,
        invocation_id: Some("active".into()),
        progress: Some(progress),
    }];
    tui.app.transcript_focus = Some(0);
    let mut monitor = MonitoredSessionState::new(SubAgentStatus::Running);
    monitor.execution_id = Some("active".into());
    tui.app.monitored_sessions.insert(key.clone(), monitor);
    tui.app.llm_busy = true;
    tui.current_prompt_abort = Some(harnx_core::abort::create_abort_signal());
    let targets: Targets = Default::default();
    tui.set_exit_cancel_factory(Arc::new({
        let targets = targets.clone();
        move |_, _, session, _, expected| {
            targets.lock().unwrap().push((session, expected));
            Box::pin(async { Ok(harnx_execution_control::CancelReceipt::idle()) })
        }
    }));
    (tui, key, targets)
}

#[tokio::test]
async fn focused_child_stop_uses_expected_generation_and_preserves_parent() {
    let (mut tui, key, targets) = child_tui().await;
    assert!(tui.cancel_selected_child());
    assert_eq!(
        *targets.lock().unwrap(),
        vec![(key.session_id, Some("active".into()))]
    );
    assert!(!tui.current_prompt_abort.as_ref().unwrap().aborted());
    assert!(tui.app.llm_busy);
    assert!(!tui.has_root_cancellation());
}

#[tokio::test]
async fn stale_child_row_cannot_cancel_reused_session_or_parent() {
    let (mut tui, key, targets) = child_tui().await;
    tui.app
        .monitored_sessions
        .get_mut(&key)
        .unwrap()
        .execution_id = Some("newer".into());
    assert!(tui.cancel_selected_child());
    assert!(targets.lock().unwrap().is_empty());
    assert!(!tui.current_prompt_abort.as_ref().unwrap().aborted());
    assert!(tui.cancellation.is_none());
}

#[tokio::test]
async fn fullscreen_child_takes_precedence_over_root_row_focus() {
    let (mut tui, key, targets) = child_tui().await;
    assert!(tui.open_focused_root_subagent());
    tui.app.transcript.clear();
    assert!(tui.cancel_selected_child());
    assert_eq!(
        *targets.lock().unwrap(),
        vec![(key.session_id, Some("active".into()))]
    );
}

#[tokio::test]
async fn cancelling_child_progress_can_converge_to_cancelled() {
    let (mut tui, key, _) = child_tui().await;
    for status in [
        SubAgentProgressStatus::Cancelling,
        SubAgentProgressStatus::Unconfirmed,
        SubAgentProgressStatus::Cancelled,
    ] {
        tui.handle_tui_event(TuiEvent::Agent(AgentEvent::Turn(
            TurnEvent::SubAgentProgress(subagent_progress(&key, "active", status, 10)),
        )))
        .await
        .unwrap();
    }
    assert!(matches!(
        &tui.app.transcript[0],
        TranscriptItem::SubAgentSession {
            status: SubAgentStatus::Cancelled,
            ..
        }
    ));
}
