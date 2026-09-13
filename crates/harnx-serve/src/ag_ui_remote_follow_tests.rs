use super::*;

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
            .forward_agent_event(AgentEvent::Tool(ToolEvent::Completed {
                id: "chatcmpl-tool-late".to_string(),
                output: serde_json::json!("done"),
                markdown: None,
            }))
            .await
    );
    assert!(forwarder.finalize().await);
    drop(forwarder);

    let mut chunks = vec![Bytes::from(frame_run_boundary_event(
        "RUN_STARTED",
        &thread_id,
        &run_id,
    ))];
    chunks.extend(
        tokio_stream::StreamExt::collect::<Vec<_>>(tokio_stream::wrappers::ReceiverStream::new(rx))
            .await,
    );
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
            .forward_agent_event(AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("partial".to_string())],
            }))
            .await
    );
    assert!(forwarder.finalize().await);
    drop(forwarder);

    let mut chunks = vec![Bytes::from(frame_run_boundary_event(
        "RUN_STARTED",
        &thread_id,
        &run_id,
    ))];
    chunks.extend(
        tokio_stream::StreamExt::collect::<Vec<_>>(tokio_stream::wrappers::ReceiverStream::new(rx))
            .await,
    );
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
