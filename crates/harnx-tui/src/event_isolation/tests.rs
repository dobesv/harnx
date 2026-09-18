use super::*;
use crate::{agent_event_sink::TuiAgentEventSink, test_utils::TuiTestHarness, types::TuiEvent};
use harnx_core::event::{AgentEventSink, ModelEvent, SessionEvent, ToolEvent, TurnEvent};
use harnx_runtime::nats_session::InterruptOutcome;

pub(crate) fn old_events() -> Vec<AgentEvent> {
    vec![
        AgentEvent::Model(ModelEvent::Final {
            output: "stale final".into(),
            usage: Default::default(),
        }),
        AgentEvent::Tool(ToolEvent::Completed {
            id: "tool".into(),
            output: "stale result".into(),
            markdown: None,
        }),
        AgentEvent::Tool(ToolEvent::Progress {
            id: "tool".into(),
            text: "stale progress".into(),
        }),
        AgentEvent::Turn(TurnEvent::SubAgentProgress(
            harnx_core::event::SubAgentProgress {
                invocation_id: "old-child-invocation".into(),
                agent: "child".into(),
                session_id: "child-session".into(),
                status: harnx_core::event::SubAgentProgressStatus::Running,
                elapsed_ms: 10,
                usage: Default::default(),
                tool_call_count: 5,
                title: Some("Old child title".into()),
            },
        )),
        AgentEvent::Model(ModelEvent::Error("stale error".into())),
        AgentEvent::Session(SessionEvent::LogSeqAssigned { seq: 999 }),
        AgentEvent::Turn(TurnEvent::Ended {
            outcome: Default::default(),
        }),
    ]
}

#[tokio::test]
async fn queued_g1_events_and_old_task_cleanup_cannot_mutate_g2() {
    let mut harness = TuiTestHarness::new().await;
    let tui = harness.tui();
    tui.clear_transcript();
    let old_task = harnx_core::abort::create_abort_signal();
    tui.current_prompt_abort = Some(old_task.clone());
    let old_live = tui.live_events.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = TuiAgentEventSink::for_prompt(tx, old_task.clone(), old_live.clone());
    for event in old_events() {
        sink.emit(event);
    }
    let current_task = harnx_core::abort::create_abort_signal();
    tui.current_prompt_abort = Some(current_task.clone());
    // A new prompt forks a fresh attachment; the sink above still holds the
    // one it was built with.
    tui.live_events = tui.live_events.fork();
    tui.app.llm_busy = true;
    tui.app.streaming_open = true;
    while let Ok(event) = rx.try_recv() {
        tui.handle_tui_event(event).await.unwrap();
    }
    tui.handle_tui_event(TuiEvent::PromptTaskFinished {
        task: old_task.clone(),
        error: Some("late cleanup".into()),
    })
    .await
    .unwrap();
    // Isolate the two guards: a matching task with a stale attachment, then a
    // stale task with the matching attachment. Neither can borrow the
    // other's identity.
    for event in old_events() {
        for (task, live) in [
            (current_task.clone(), old_live.clone()),
            (old_task.clone(), tui.live_events.clone()),
        ] {
            tui.handle_tui_event(TuiEvent::Agent {
                task,
                stamp: EventStamp::live(&live),
                event: event.clone(),
            })
            .await
            .unwrap();
        }
    }
    assert!(tui.app.llm_busy, "old cleanup cleared g2's spinner");
    assert!(tui.app.streaming_open);
    assert!(tui.app.transcript.is_empty());
    assert!(Arc::ptr_eq(
        tui.current_prompt_abort.as_ref().unwrap(),
        &current_task
    ));
}

#[tokio::test]
async fn accepted_interrupt_rejects_buffered_output_and_the_fence_survives_reconnect() {
    let mut harness = TuiTestHarness::new().await;
    let tui = harness.tui();
    tui.clear_transcript();
    let task = harnx_core::abort::create_abort_signal();
    tui.current_prompt_abort = Some(task.clone());
    // monitor_interrupt only fences/settles a tray targeting the session
    // this Tui is actually driving; give it a root tray to match.
    tui.active_remote_session = Some(("session".to_string(), "cluster".to_string()));
    tui.cancellation = Some(crate::cancellation::CancellationTray {
        phase: crate::cancellation::CancellationPhase::Requesting,
        session_id: "session".to_string(),
        cluster: "cluster".to_string(),
        editor_restored: false,
    });
    let stamp = EventStamp::snapshot(&tui.live_events);
    tui.monitor_interrupt(InterruptOutcome::Accepted { cancel_seq: 5 });
    assert!(
        tui.current_prompt_abort.is_none(),
        "settling an accepted interrupt retires the prompt task"
    );
    tui.app.llm_busy = true; // A settled prompt does not enable early return/overlap.
    for event in old_events() {
        tui.handle_tui_event(TuiEvent::Agent {
            task: task.clone(),
            stamp: stamp.clone(),
            event,
        })
        .await
        .unwrap();
    }
    assert!(tui.app.llm_busy);
    assert!(tui.app.transcript.is_empty());

    // A reconnect (fork) keeps the same cancel fence, so a pre-interrupt
    // advisory still cannot render even from a brand-new attachment.
    let reattached = tui.live_events.fork();
    let stale = harnx_runtime::nats_event_sink::AdvisoryEnvelope::new(
        3,
        AgentEvent::Notice(harnx_core::event::NoticeEvent::Info("stale".into())),
    );
    assert!(
        !reattached.should_render(&stale, 0),
        "reconnect lost the accepted interrupt fence"
    );
}

#[tokio::test]
async fn shared_live_and_durable_cleanup_are_attachment_checked_at_render() {
    let mut harness = TuiTestHarness::new().await;
    let tui = harness.tui();
    tui.clear_transcript();
    tui.session_activity_target = Some(("session".into(), "cluster".into()));
    let old = EventStamp::snapshot(&tui.live_events);
    // A fresh attachment (e.g. a new prompt) replaces `tui.live_events`; `old`
    // still points at the one that came before it.
    tui.live_events = tui.live_events.fork();
    tui.app.llm_busy = true;
    tui.app.streaming_open = true;
    for event in old_events() {
        tui.handle_tui_event(TuiEvent::SessionAgent {
            session_id: "session".into(),
            cluster: "cluster".into(),
            stamp: old.clone(),
            historical: false,
            event,
        })
        .await
        .unwrap();
    }
    for historical in [false, true] {
        tui.handle_tui_event(TuiEvent::SessionActivity {
            session_id: "session".into(),
            cluster: "cluster".into(),
            stamp: old.clone(),
            historical,
            active: false,
        })
        .await
        .unwrap();
    }
    assert!(tui.app.llm_busy);
    assert!(tui.app.streaming_open);
    assert!(tui.app.transcript.is_empty());
}

#[tokio::test]
async fn old_child_events_cannot_replace_child_g2() {
    use crate::types::{MonitoredSessionKey, MonitoredSessionState, SubAgentStatus};
    let mut harness = TuiTestHarness::new().await;
    let tui = harness.tui();
    let key = MonitoredSessionKey {
        agent: "child".into(),
        session_id: "child-session".into(),
        cluster: "cluster".into(),
    };
    // A stamp captured for an earlier, never-inserted attachment must not
    // mutate the child's actual, separately attached G2 state below.
    let old = EventStamp::live(&tui.live_events.fork());
    let mut state = MonitoredSessionState::new(SubAgentStatus::Running);
    state.execution_id = Some("child-g2".into());
    state.invocation_id = Some("child-g2".into());
    state.streaming_open = true;
    tui.app.monitored_sessions.insert(key.clone(), state);
    for event in old_events() {
        tui.handle_tui_event(TuiEvent::SubAgentSessionEvent {
            key: key.clone(),
            stamp: old.clone(),
            event,
        })
        .await
        .unwrap();
    }
    let child = &tui.app.monitored_sessions[&key];
    assert_eq!(child.execution_id.as_deref(), Some("child-g2"));
    assert_eq!(child.status, SubAgentStatus::Running);
    assert!(child.streaming_open);
    assert!(child.transcript.is_empty());
}
