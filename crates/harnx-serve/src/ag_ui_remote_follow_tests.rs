use super::*;
use crate::ag_ui_remote_follow::{event_frames, AdvisoryForwarder, QueuedEvent};
use harnx_runtime::nats_event_sink::LiveEventState;

/// Queue the frames of one text chunk the way a live advisory carrying
/// `after_seq` would, then close the queue so a reader sees exactly those.
async fn queued_chunk(after_seq: u64, text: &str) -> tokio::sync::mpsc::Receiver<QueuedEvent> {
    use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent};

    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let mut forwarder = AdvisoryForwarder::new(tx);
    assert!(
        forwarder
            .forward_agent_event(
                after_seq,
                AgentEvent::Model(ModelEvent::MessageChunk {
                    blocks: vec![ContentBlock::Text(text.into())],
                })
            )
            .await,
        "the output queue must accept the chunk"
    );
    rx
}

/// Everything the queue lets through to a reader with this fence, at the
/// durable position it has already applied.
async fn drained_frames(
    rx: tokio::sync::mpsc::Receiver<QueuedEvent>,
    live: LiveEventState,
    last_durable_seq: u64,
) -> Vec<Bytes> {
    tokio_stream::StreamExt::collect(event_frames(rx, live, last_durable_seq)).await
}

async fn collect_remote_frames(rx: tokio::sync::mpsc::Receiver<QueuedEvent>) -> Vec<Bytes> {
    drained_frames(rx, LiveEventState::default(), 0).await
}

#[tokio::test]
async fn completed_remote_path_orders_boundary_before_hydrated_handoff() {
    use harnx_core::session::SessionLogEntry;

    let thread_id = Uuid::new_v4().to_string();
    let run_id = Uuid::new_v4().to_string();
    let log_entries = vec![(
        1u64,
        SessionLogEntry::HandoffCommitted {
            target_agent: "remote-agent".to_string(),
            target_session_id: "remote-session-456".to_string(),
            handoff_tool_call_id: Some("call-remote".to_string()),
        },
    )];
    let control_frames = control_snapshot_events(&log_entries, None)
        .into_iter()
        .filter_map(|event| frame_event(&event).ok().map(Bytes::from))
        .collect();
    let initial_frames = [
        Bytes::from(frame_run_boundary_event("RUN_STARTED", &thread_id, &run_id)),
        crate::ag_ui_attach::session_attach_boundary_frame(1),
    ];
    let snapshot_frame = Some(Bytes::from(
        frame_event(&snapshot_event(vec![user_msg("remote test")])).expect("snapshot frame"),
    ));

    let stream = crate::ag_ui_remote_follow::completed_remote_stream(
        initial_frames,
        snapshot_frame,
        control_frames,
        &thread_id,
        &run_id,
    );
    let events = decode_sse_bytes_chunks(tokio_stream::StreamExt::collect::<Vec<_>>(stream).await);

    assert_event_type_sequence(
        &events,
        &[
            "RUN_STARTED",
            "CUSTOM",
            "MESSAGES_SNAPSHOT",
            "CUSTOM",
            "RUN_FINISHED",
        ],
    );
    assert_attach_boundary(&events[1], 1);
    let handoff = &events[3];
    assert_eq!(handoff["name"], "session_handoff");
    assert_eq!(handoff["value"]["agent"], "remote-agent");
    assert_eq!(handoff["value"]["session_id"], "remote-session-456");
    assert_eq!(handoff["value"]["handoff_tool_call_id"], "call-remote");
    assert_eq!(handoff["value"]["after_seq"], 1);
}

fn assert_strict_lifecycle_valid(events: &[serde_json::Value]) {
    use std::collections::HashSet;

    let mut text = HashSet::new();
    let mut tools = HashSet::new();
    let mut steps = HashSet::new();
    let mut thinking = false;
    let mut thinking_text = false;

    for event in events {
        match event["type"].as_str().expect("event type") {
            "TEXT_MESSAGE_START" => {
                text.insert(event["messageId"].as_str().expect("message id").to_string());
            }
            "TEXT_MESSAGE_END" => assert!(
                text.remove(event["messageId"].as_str().expect("message id")),
                "orphan text end: {event:?}"
            ),
            "TOOL_CALL_START" => {
                tools.insert(
                    event["toolCallId"]
                        .as_str()
                        .expect("tool call id")
                        .to_string(),
                );
            }
            "TOOL_CALL_END" => assert!(
                tools.remove(event["toolCallId"].as_str().expect("tool call id")),
                "orphan tool end: {event:?}"
            ),
            "STEP_STARTED" => {
                steps.insert(event["stepName"].as_str().expect("step name").to_string());
            }
            "STEP_FINISHED" => assert!(
                steps.remove(event["stepName"].as_str().expect("step name")),
                "orphan step finish: {event:?}"
            ),
            "THINKING_START" => thinking = true,
            "THINKING_TEXT_MESSAGE_START" => {
                assert!(thinking, "thinking text opened outside thinking segment");
                thinking_text = true;
            }
            "THINKING_TEXT_MESSAGE_END" => {
                assert!(thinking_text, "orphan thinking text end");
                thinking_text = false;
            }
            "THINKING_END" => {
                assert!(thinking, "orphan thinking end");
                assert!(!thinking_text, "thinking ended before thinking text");
                thinking = false;
            }
            "RUN_FINISHED" | "RUN_ERROR" => {
                assert!(text.is_empty(), "active text at terminal: {text:?}");
                assert!(tools.is_empty(), "active tools at terminal: {tools:?}");
                assert!(steps.is_empty(), "active steps at terminal: {steps:?}");
                assert!(!thinking_text, "active thinking text at terminal");
                assert!(!thinking, "active thinking at terminal");
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn remote_attach_suppresses_completed_tool_tail_without_forwarded_start() {
    use harnx_core::event::{AgentEvent, ToolEvent};

    let thread_id = Uuid::new_v4().to_string();
    let run_id = Uuid::new_v4().to_string();
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let mut forwarder = crate::ag_ui_remote_follow::AdvisoryForwarder::new(tx);

    assert!(
        forwarder
            .forward_agent_event(
                u64::MAX,
                AgentEvent::Tool(ToolEvent::Completed {
                    id: "chatcmpl-tool-late".to_string(),
                    output: serde_json::json!("done"),
                    markdown: None,
                })
            )
            .await
    );
    drop(forwarder);

    let mut chunks = vec![Bytes::from(frame_run_boundary_event(
        "RUN_STARTED",
        &thread_id,
        &run_id,
    ))];
    chunks.extend(collect_remote_frames(rx).await);
    chunks.push(Bytes::from(frame_run_boundary_event(
        "RUN_FINISHED",
        &thread_id,
        &run_id,
    )));
    let events = decode_sse_bytes_chunks(chunks);

    assert!(
        !events.iter().any(|event| matches!(
            event["type"].as_str(),
            Some("TOOL_CALL_END" | "TOOL_CALL_RESULT")
        )),
        "snapshot-unknown tool tail must be suppressed: {events:?}"
    );
    assert_strict_lifecycle_valid(&events);
}

#[tokio::test]
async fn remote_poll_terminal_closes_text_when_final_advisory_is_missing() {
    use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent};

    let thread_id = Uuid::new_v4().to_string();
    let run_id = Uuid::new_v4().to_string();
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let mut forwarder = crate::ag_ui_remote_follow::AdvisoryForwarder::new(tx);

    assert!(
        forwarder
            .forward_agent_event(
                u64::MAX,
                AgentEvent::Model(ModelEvent::MessageChunk {
                    blocks: vec![ContentBlock::Text("partial".to_string())],
                })
            )
            .await
    );
    drop(forwarder);

    let mut chunks = vec![Bytes::from(frame_run_boundary_event(
        "RUN_STARTED",
        &thread_id,
        &run_id,
    ))];
    chunks.extend(collect_remote_frames(rx).await);
    chunks.push(Bytes::from(frame_run_boundary_event(
        "RUN_FINISHED",
        &thread_id,
        &run_id,
    )));
    let events = decode_sse_bytes_chunks(chunks);
    let text_end = events
        .iter()
        .position(|event| event["type"] == "TEXT_MESSAGE_END")
        .expect("terminal finalization must close text");
    let run_finished = events
        .iter()
        .position(|event| event["type"] == "RUN_FINISHED")
        .expect("run finished");

    assert!(
        text_end < run_finished,
        "text must close before run terminal"
    );
    assert_strict_lifecycle_valid(&events);
}

/// A reader that detached before the queue drained sends nothing at all, so
/// the START it never sent cannot leave an END behind.
#[tokio::test]
async fn queued_remote_start_is_discarded_without_an_orphan_end_after_stop() {
    let rx = queued_chunk(u64::MAX, "queued before stop").await;
    let live = LiveEventState::default();
    live.retire();

    let frames = drained_frames(rx, live, 0).await;
    assert!(
        frames.is_empty(),
        "unsent START must not produce a synthetic END"
    );
}

/// The queue re-checks the fence when an event reaches the wire, not when it is
/// enqueued. Here nothing is retired and the chunk clears the reader's durable
/// position, so the accepted `Cancel` is the only thing that can drop it.
#[tokio::test]
async fn the_accepted_cancel_alone_drops_a_chunk_queued_below_it() {
    let rx = queued_chunk(5, "output the Cancel ended").await;
    let live = LiveEventState::default();
    live.accept_interrupt(9);

    let frames = drained_frames(rx, live, 5).await;
    assert!(
        frames.is_empty(),
        "an advisory from below the Cancel must not reach the wire: {frames:?}"
    );
}

/// Detaching midway is the other half: the lifecycle already on the wire has to
/// be closed, and only that one.
#[tokio::test]
async fn stopped_remote_queue_closes_only_the_lifecycle_already_sent() {
    let rx = queued_chunk(u64::MAX, "not sent before stop").await;
    let live = LiveEventState::default();
    let stream = event_frames(rx, live.clone(), 0);
    tokio::pin!(stream);
    let start = tokio_stream::StreamExt::next(&mut stream).await.unwrap();
    live.retire();
    let mut frames = vec![start];
    frames.extend(tokio_stream::StreamExt::collect::<Vec<_>>(stream).await);
    frames.push(Bytes::from(frame_run_boundary_event(
        "RUN_FINISHED",
        &Uuid::new_v4().to_string(),
        &Uuid::new_v4().to_string(),
    )));
    let events = decode_sse_bytes_chunks(frames);
    assert_event_type_sequence(
        &events,
        &["TEXT_MESSAGE_START", "TEXT_MESSAGE_END", "RUN_FINISHED"],
    );
    assert_strict_lifecycle_valid(&events);
}
