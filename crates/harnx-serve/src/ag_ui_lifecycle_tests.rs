use super::*;

fn open_all_lifecycles() -> (LiveStreamGuard, Vec<Bytes>) {
    let message_id = MessageId::random();
    let tool_call_id = ToolCallId::random();
    let base = || BaseEvent {
        timestamp: None,
        raw_event: None,
    };
    let mut guard = LiveStreamGuard::default();
    let chunks = vec![
        Event::TextMessageStart(TextMessageStartEvent {
            base: base(),
            message_id: message_id.clone(),
            role: Role::Assistant,
        }),
        Event::StepStarted(StepStartedEvent {
            base: base(),
            step_name: "turn-7".to_string(),
        }),
        Event::ToolCallStart(ToolCallStartEvent {
            base: base(),
            tool_call_id,
            tool_call_name: "search".to_string(),
            parent_message_id: Some(message_id),
        }),
        Event::ThinkingStart(ThinkingStartEvent {
            base: base(),
            title: None,
        }),
        Event::ThinkingTextMessageStart(ThinkingTextMessageStartEvent { base: base() }),
    ]
    .into_iter()
    .filter_map(|event| frame_guarded_live_event(event, &mut guard))
    .collect();
    (guard, chunks)
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
                text.insert(event["messageId"].as_str().expect("message id"));
            }
            "TEXT_MESSAGE_END" => assert!(
                text.remove(event["messageId"].as_str().expect("message id")),
                "orphan text end: {event:?}"
            ),
            "TOOL_CALL_START" => {
                tools.insert(event["toolCallId"].as_str().expect("tool call id"));
            }
            "TOOL_CALL_END" => assert!(
                tools.remove(event["toolCallId"].as_str().expect("tool call id")),
                "orphan tool end: {event:?}"
            ),
            "STEP_STARTED" => {
                steps.insert(event["stepName"].as_str().expect("step name"));
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

#[test]
fn live_terminal_finalization_closes_every_open_lifecycle_before_run_finished() {
    let thread_id = ThreadId::random();
    let run_id = RunId::random();
    let base = || BaseEvent {
        timestamp: None,
        raw_event: None,
    };
    let (mut guard, mut chunks) = open_all_lifecycles();
    let mut state = FirstRunState::Active;
    chunks.push(
        frame_live_event(
            Event::RunFinished(ag_ui_core::event::RunFinishedEvent {
                base: base(),
                thread_id: thread_id.clone(),
                run_id: run_id.clone(),
                result: None,
            }),
            &mut state,
            &mut guard,
            &thread_id.to_string(),
            &run_id.to_string(),
        )
        .expect("terminal frame"),
    );

    let events = decode_sse_bytes_chunks(chunks);
    assert_event_type_sequence(
        &events,
        &[
            "TEXT_MESSAGE_START",
            "STEP_STARTED",
            "TOOL_CALL_START",
            "THINKING_START",
            "THINKING_TEXT_MESSAGE_START",
            "TEXT_MESSAGE_END",
            "STEP_FINISHED",
            "TOOL_CALL_END",
            "THINKING_TEXT_MESSAGE_END",
            "THINKING_END",
            "RUN_FINISHED",
        ],
    );
    assert_strict_lifecycle_valid(&events);
}

#[test]
fn live_terminal_finalization_closes_every_open_lifecycle_before_run_error() {
    let thread_id = ThreadId::random();
    let run_id = RunId::random();
    let base = || BaseEvent {
        timestamp: None,
        raw_event: None,
    };
    let (mut guard, mut chunks) = open_all_lifecycles();
    let mut state = FirstRunState::Active;
    chunks.push(
        frame_live_event(
            Event::RunError(ag_ui_core::event::RunErrorEvent {
                base: base(),
                message: "model failed".to_string(),
                code: Some("provider_error".to_string()),
            }),
            &mut state,
            &mut guard,
            &thread_id.to_string(),
            &run_id.to_string(),
        )
        .expect("error terminal frame"),
    );

    let events = decode_sse_bytes_chunks(chunks);
    assert_event_type_sequence(
        &events,
        &[
            "TEXT_MESSAGE_START",
            "STEP_STARTED",
            "TOOL_CALL_START",
            "THINKING_START",
            "THINKING_TEXT_MESSAGE_START",
            "TEXT_MESSAGE_END",
            "STEP_FINISHED",
            "TOOL_CALL_END",
            "THINKING_TEXT_MESSAGE_END",
            "THINKING_END",
            "RUN_ERROR",
        ],
    );
    assert_strict_lifecycle_valid(&events);
}

#[test]
fn live_snapshot_reconciliation_closes_and_resets_guard_before_terminal() {
    let thread_id = ThreadId::random();
    let run_id = RunId::random();
    let message_id = MessageId::random();
    let tool_call_id = ToolCallId::random();
    let base = || BaseEvent {
        timestamp: None,
        raw_event: None,
    };
    let mut guard = LiveStreamGuard::default();
    let mut chunks = vec![
        Event::TextMessageStart(TextMessageStartEvent {
            base: base(),
            message_id: message_id.clone(),
            role: Role::Assistant,
        }),
        Event::StepStarted(StepStartedEvent {
            base: base(),
            step_name: "turn-lagged".to_string(),
        }),
        Event::ToolCallStart(ToolCallStartEvent {
            base: base(),
            tool_call_id,
            tool_call_name: "search".to_string(),
            parent_message_id: Some(message_id),
        }),
    ]
    .into_iter()
    .filter_map(|event| frame_guarded_live_event(event, &mut guard))
    .collect::<Vec<_>>();

    chunks.push(
        frame_guarded_live_event(
            snapshot_event(vec![user_msg("history after broadcast lag")]),
            &mut guard,
        )
        .expect("lag replacement snapshot"),
    );
    let mut state = FirstRunState::Active;
    chunks.push(
        frame_live_event(
            Event::RunFinished(ag_ui_core::event::RunFinishedEvent {
                base: base(),
                thread_id: thread_id.clone(),
                run_id: run_id.clone(),
                result: None,
            }),
            &mut state,
            &mut guard,
            &thread_id.to_string(),
            &run_id.to_string(),
        )
        .expect("terminal frame"),
    );

    let events = decode_sse_bytes_chunks(chunks);
    assert_event_type_sequence(
        &events,
        &[
            "TEXT_MESSAGE_START",
            "STEP_STARTED",
            "TOOL_CALL_START",
            "TEXT_MESSAGE_END",
            "STEP_FINISHED",
            "TOOL_CALL_END",
            "MESSAGES_SNAPSHOT",
            "RUN_FINISHED",
        ],
    );
}
