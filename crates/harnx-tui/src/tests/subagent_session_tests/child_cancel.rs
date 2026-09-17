use super::*;
use crate::types::{MonitoredSessionState, SubAgentInvocationProgress, Tui};
use harnx_runtime::config::LOCAL_CLUSTER_KEY;
use std::sync::{Arc, Mutex};

type Targets = Arc<Mutex<Vec<String>>>;

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
    // The parent's own turn, distinct from the child's storage key below —
    // an accepted child interrupt must never fence or settle it.
    tui.active_remote_session = Some(("parent-session".into(), LOCAL_CLUSTER_KEY.into()));
    let targets: Targets = Default::default();
    tui.set_exit_cancel_factory(Arc::new({
        let targets = targets.clone();
        move |_, _, session, _| {
            targets.lock().unwrap().push(session);
            Box::pin(async {
                Ok(harnx_runtime::nats_session::InterruptOutcome::Accepted { cancel_seq: 42 })
            })
        }
    }));
    (tui, key, targets)
}

#[tokio::test]
async fn focused_child_stop_targets_the_child_session_and_preserves_parent() {
    let (mut tui, key, targets) = child_tui().await;
    assert!(tui.cancel_selected_child());
    assert_eq!(*targets.lock().unwrap(), vec![key.storage_key()]);
    // One durable interrupt append per session: cancelling a child shows the
    // same tray a root interrupt would, without touching the parent's own
    // follower task.
    assert!(tui.has_root_cancellation());

    tui.poll_pending_exit_cancel().await;

    // The child's own interrupt is fully accepted, but it targets a
    // different session than the one this Tui is actively driving, so it
    // must never fence or settle the parent's turn.
    assert!(!tui.current_prompt_abort.as_ref().unwrap().aborted());
    assert!(tui.app.llm_busy);
    assert_eq!(
        tui.active_remote_session,
        Some(("parent-session".into(), LOCAL_CLUSTER_KEY.into()))
    );
    let probe = harnx_runtime::nats_event_sink::AdvisoryEnvelope::new(
        0,
        AgentEvent::Notice(harnx_core::event::NoticeEvent::Info("probe".into())),
    );
    assert!(
        tui.live_events.should_render(&probe, 0),
        "a child interrupt must not fence the parent's live state"
    );
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
    assert_eq!(*targets.lock().unwrap(), vec![key.storage_key()]);
}

#[tokio::test]
async fn cancelling_child_progress_can_converge_to_cancelled() {
    let (mut tui, key, _) = child_tui().await;
    for status in [
        SubAgentProgressStatus::Cancelling,
        SubAgentProgressStatus::Unconfirmed,
        SubAgentProgressStatus::Cancelled,
    ] {
        tui.handle_tui_event(TuiEvent::LocalAgent(AgentEvent::Turn(
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
