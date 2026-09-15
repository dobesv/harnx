use super::*;
use crate::{agent_event_sink::TuiAgentEventSink, test_utils::TuiTestHarness, types::TuiEvent};
use harnx_core::event::{AgentEventSink, ModelEvent, SessionEvent, ToolEvent, TurnEvent};

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
    tui.live_events.select(Some("g1".into()));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink =
        TuiAgentEventSink::for_prompt(tx, old_task.clone(), tui.live_events.clone(), "g1".into());
    for event in old_events() {
        sink.emit_live(event, "g1");
    }
    let current_task = harnx_core::abort::create_abort_signal();
    tui.current_prompt_abort = Some(current_task.clone());
    tui.live_events.select(Some("g2".into()));
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
    // Isolate the two guards: a matching task with stale origin, then a stale
    // task with a matching origin. Neither can borrow the other's identity.
    for event in old_events() {
        for (task, generation) in [(current_task.clone(), "g1"), (old_task.clone(), "g2")] {
            tui.handle_tui_event(TuiEvent::Agent {
                task,
                stamp: EventStamp::live(&tui.live_events, Some(generation.into())),
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
async fn delayed_publisher_checks_generation_at_enqueue() {
    let state = LiveEventState::default();
    state.select(Some("g1".into()));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = TuiAgentEventSink::for_prompt(
        tx,
        harnx_core::abort::create_abort_signal(),
        state.clone(),
        "g1".into(),
    );
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let publisher = tokio::spawn(async move {
        ready_tx.send(()).unwrap();
        release_rx.await.unwrap();
        for event in old_events() {
            sink.emit_live(event, "g1");
        }
    });
    ready_rx.await.unwrap();
    let replacement = state.replacement();
    replacement.select(Some("g2".into()));
    // A delayed reconnect from the detached reader cannot make it current again.
    state.select(Some("g1".into()));
    release_tx.send(()).unwrap();
    publisher.await.unwrap();
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn accepted_receipt_rejects_buffered_output_before_cleanup_finishes() {
    let mut harness = TuiTestHarness::new().await;
    let tui = harness.tui();
    tui.clear_transcript();
    let task = harnx_core::abort::create_abort_signal();
    tui.current_prompt_abort = Some(task.clone());
    tui.live_events.select(Some("g1".into()));
    let stamp = EventStamp::snapshot(&tui.live_events);
    let mut receipt = harnx_execution_control::CancelReceipt::idle();
    receipt.execution_id = Some("g1".into());
    receipt.cancellation_id = Some("accepted-stop".into());
    receipt.cancelled = true;
    receipt.disposition = harnx_execution_control::CancelDisposition::Requested;
    tui.monitor_cancellation(receipt);
    assert!(!tui.live_events.allows(Some("g1")));
    tui.app.llm_busy = true; // Stage 6 does not enable early return/overlap.
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
    let reattached = tui.live_events.fork();
    reattached.select(Some("g1".into()));
    assert!(
        !reattached.allows(Some("g1")),
        "reconnect lost locally accepted stop"
    );
}

#[tokio::test]
async fn shared_live_and_durable_cleanup_are_generation_checked_at_render() {
    let mut harness = TuiTestHarness::new().await;
    let tui = harness.tui();
    tui.clear_transcript();
    tui.session_activity_target = Some(("session".into(), "cluster".into()));
    tui.live_events.select(Some("g1".into()));
    let old = EventStamp::snapshot(&tui.live_events);
    tui.live_events.select(Some("g2".into()));
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
    // A queued advisory from the current generation also loses permission as
    // soon as its stop receipt is accepted, even with active=false.
    let stopped = EventStamp::snapshot(&tui.live_events);
    tui.live_events.stop("g2");
    tui.handle_tui_event(TuiEvent::SessionActivity {
        session_id: "session".into(),
        cluster: "cluster".into(),
        stamp: stopped,
        historical: false,
        active: false,
    })
    .await
    .unwrap();
    assert!(tui.app.llm_busy);
}

#[tokio::test]
async fn old_child_events_and_execution_cleanup_cannot_replace_child_g2() {
    use crate::types::{MonitoredSessionKey, MonitoredSessionState, SubAgentStatus};
    let mut harness = TuiTestHarness::new().await;
    let tui = harness.tui();
    let key = MonitoredSessionKey {
        agent: "child".into(),
        session_id: "child-session".into(),
        cluster: "cluster".into(),
    };
    let mut state = MonitoredSessionState::new(SubAgentStatus::Running);
    state.live_events = tui.live_events.fork();
    state.live_events.select(Some("child-g2".into()));
    state.execution_id = Some("child-g2".into());
    state.invocation_id = Some("child-g2".into());
    state.streaming_open = true;
    let old = EventStamp::live(&state.live_events, Some("child-g1".into()));
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
    let mut operation = harnx_execution_control::Operation::preparing(
        harnx_execution_control::OperationRef::new("child-session", "child-g1"),
        harnx_execution_control::OperationKind::Session,
        None,
    );
    operation.request_cancel("old-stop", false).unwrap();
    tui.hydrate_execution_state("cluster".into(), operation);
    let child = &tui.app.monitored_sessions[&key];
    assert_eq!(child.execution_id.as_deref(), Some("child-g2"));
    assert_eq!(child.status, SubAgentStatus::Running);
    assert!(child.streaming_open);
    assert!(child.transcript.is_empty());
}

#[tokio::test]
async fn execution_snapshot_cannot_request_cancellation_before_fence_load_or_after_g2() {
    let mut harness = TuiTestHarness::new().await;
    let tui = harness.tui();
    tui.session_activity_target = Some(("session".into(), "cluster".into()));
    let mut operation = harnx_execution_control::Operation::preparing(
        harnx_execution_control::OperationRef::new("session", "g1"),
        harnx_execution_control::OperationKind::Session,
        None,
    );
    operation.request_cancel("old-stop", false).unwrap();
    tui.hydrate_execution_state("cluster".into(), operation.clone());
    assert!(tui.pending_exit_cancel.is_none());
    tui.live_events.select(Some("g2".into()));
    tui.hydrate_execution_state("cluster".into(), operation);
    assert!(tui.pending_exit_cancel.is_none());
    assert!(tui.cancellation.is_none());
}
