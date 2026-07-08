use super::*;
use crate::test_support::TestConfigSandbox;
use anyhow::anyhow;
use harnx_core::{
    api_types::CompletionTokenUsage,
    event::{AgentEventSink, ModelEvent, SessionEvent, TurnEvent},
    message::MessageContentToolCalls,
};
use harnx_runtime::{client::ToolCall, config::Config};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tokio_stream::StreamExt;

fn assert_event_type_sequence(events: &[Value], expected: &[&str]) {
    let event_types = events
        .iter()
        .map(|event| event["type"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(event_types, expected);
}

fn ag_ui_test_registry(
    config: &Config,
    call_fn: Option<AgentCallFn>,
) -> crate::session_actor::SessionRegistry {
    crate::session_actor::SessionRegistry::new_for_tests(
        config.clone(),
        std::time::Duration::from_secs(30),
        call_fn,
    )
}

fn ag_ui_request_body(run_id: impl serde::Serialize, content: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "threadId": Uuid::new_v4(),
        "runId": run_id,
        "state": {},
        "messages": [{
            "id": Uuid::new_v4(),
            "role": "user",
            "content": content
        }],
        "tools": [],
        "context": [],
        "forwardedProps": {}
    }))
    .unwrap()
}

#[test]
fn ag_ui_sink_emits_text_message_content_for_chunk() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id =
        MessageId::from(uuid::Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap());
    let sink = super::AgUiSink::with_snapshot(tx, message_id.clone(), false, None);

    sink.emit(
        AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("hello".to_string())],
        }),
        None,
    );

    match rx.try_recv().expect("text start") {
        Event::TextMessageStart(event) => {
            assert_eq!(event.message_id, message_id.clone());
            assert_eq!(event.role, Role::Assistant);
        }
        other => panic!("expected TextMessageStart event, got: {other:?}"),
    }
    match rx.try_recv().expect("text content") {
        Event::TextMessageContent(TextMessageContentEvent {
            base,
            message_id: mid,
            delta,
        }) => {
            assert_eq!(base.timestamp, None);
            assert_eq!(base.raw_event, None);
            assert_eq!(mid, message_id);
            assert_eq!(delta, "hello");
        }
        other => panic!("expected TextMessageContent event, got: {other:?}"),
    }

    assert!(rx.try_recv().is_err());
}

#[test]
fn ag_ui_sink_skips_empty_chunk() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::new(tx, message_id);

    sink.emit(
        AgentEvent::Model(ModelEvent::MessageChunk { blocks: vec![] }),
        None,
    );
    assert!(rx.try_recv().is_err());

    sink.emit(
        AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Image {
                data: vec![],
                mime: "image/png".to_string(),
            }],
        }),
        None,
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn ag_ui_sink_emits_run_error_for_model_error() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::new(tx, message_id);

    sink.emit(
        AgentEvent::Model(ModelEvent::Error("boom".to_string())),
        None,
    );

    let event = rx.try_recv().expect("should receive event");
    match event {
        Event::RunError(RunErrorEvent {
            base,
            message,
            code,
        }) => {
            assert_eq!(base.timestamp, None);
            assert_eq!(base.raw_event, None);
            assert_eq!(message, "boom");
            assert!(code.is_none());
        }
        _ => panic!("expected RunError event, got: {:?}", event),
    }
}

#[test]
fn ag_ui_sink_emits_run_error_for_notice_error() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::new(tx, message_id);

    sink.emit(
        AgentEvent::Notice(NoticeEvent::Error("fatal error".to_string())),
        None,
    );

    let event = rx.try_recv().expect("should receive event");
    match event {
        Event::RunError(RunErrorEvent { message, .. }) => {
            assert_eq!(message, "fatal error");
        }
        _ => panic!("expected RunError event, got: {:?}", event),
    }
}

#[test]
fn ag_ui_sink_emits_custom_status_event() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::new(tx, message_id);

    sink.emit(
        AgentEvent::Status(harnx_core::event::StatusLine {
            text: "working...".to_string(),
        }),
        None,
    );

    let event = rx.try_recv().expect("should receive status event");
    match event {
        Event::Custom(CustomEvent { base, name, value }) => {
            assert_eq!(base.timestamp, None);
            assert_eq!(base.raw_event, None);
            assert_eq!(name, "status");
            assert_eq!(value["text"].as_str(), Some("working..."));
        }
        _ => panic!("expected Custom event with name 'status', got: {:?}", event),
    }
    assert!(rx.try_recv().is_err());
}

#[test]
fn ag_ui_sink_emits_final_as_content_when_non_empty() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::with_snapshot(tx, message_id.clone(), false, None);

    let usage = CompletionTokenUsage::new(Some(10), Some(5), Some(0));
    sink.emit(
        AgentEvent::Model(ModelEvent::Final {
            output: "final text".to_string(),
            usage,
        }),
        None,
    );

    match rx.try_recv().expect("text start") {
        Event::TextMessageStart(event) => {
            assert_eq!(event.message_id, message_id.clone());
            assert_eq!(event.role, Role::Assistant);
        }
        other => panic!("expected TextMessageStart event, got: {other:?}"),
    }
    match rx.try_recv().expect("text content") {
        Event::TextMessageContent(TextMessageContentEvent {
            base,
            message_id: mid,
            delta,
        }) => {
            assert_eq!(mid, message_id);
            assert_eq!(delta, "final text");
            assert_eq!(base.timestamp, None);
        }
        other => panic!("expected TextMessageContent event, got: {other:?}"),
    }
}

#[test]
fn ag_ui_sink_skips_final_when_empty() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::new(tx, message_id);

    let usage = CompletionTokenUsage::new(Some(10), Some(5), Some(0));
    sink.emit(
        AgentEvent::Model(ModelEvent::Final {
            output: String::new(),
            usage,
        }),
        None,
    );

    assert!(rx.try_recv().is_err());
}

#[test]
fn ag_ui_sink_maps_thought_then_text_and_closes_thinking_before_text() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::with_snapshot(tx, message_id.clone(), false, None);

    sink.emit(
        AgentEvent::Model(ModelEvent::ThoughtChunk {
            blocks: vec![ContentBlock::Text("thinking...".to_string())],
        }),
        None,
    );
    sink.emit(
        AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("answer".to_string())],
        }),
        None,
    );
    sink.emit(
        AgentEvent::Model(ModelEvent::Final {
            output: String::new(),
            usage: CompletionTokenUsage::default(),
        }),
        None,
    );

    assert!(matches!(
        rx.try_recv().expect("thinking start"),
        Event::ThinkingStart(_)
    ));
    match rx.try_recv().expect("thinking text start") {
        Event::ThinkingTextMessageStart(event) => assert_eq!(event.base.timestamp, None),
        other => panic!("expected ThinkingTextMessageStart, got: {other:?}"),
    }
    match rx.try_recv().expect("thinking delta") {
        Event::ThinkingTextMessageContent(event) => assert_eq!(event.delta, "thinking..."),
        other => panic!("expected ThinkingTextMessageContent, got: {other:?}"),
    }
    assert!(matches!(
        rx.try_recv().expect("thinking end"),
        Event::ThinkingTextMessageEnd(_)
    ));
    assert!(matches!(
        rx.try_recv().expect("thinking close"),
        Event::ThinkingEnd(_)
    ));
    match rx.try_recv().expect("text start") {
        Event::TextMessageStart(event) => {
            assert_eq!(event.message_id, message_id);
            assert_eq!(event.role, Role::Assistant);
        }
        other => panic!("expected TextMessageStart, got: {other:?}"),
    }
    match rx.try_recv().expect("text delta") {
        Event::TextMessageContent(event) => {
            assert_eq!(event.message_id, message_id);
            assert_eq!(event.delta, "answer");
        }
        other => panic!("expected TextMessageContent, got: {other:?}"),
    }
    assert!(rx.try_recv().is_err());
}

#[test]
fn ag_ui_sink_closes_thinking_before_tool_events() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::with_snapshot(tx, message_id.clone(), false, None);

    sink.emit(
        AgentEvent::Model(ModelEvent::ThoughtChunk {
            blocks: vec![ContentBlock::Text("planning".to_string())],
        }),
        None,
    );
    sink.emit(
        AgentEvent::Tool(ToolEvent::Started {
            id: "tool-1".to_string(),
            name: "read_history".to_string(),
            kind: harnx_core::event::ToolKind::Other,
            markdown: None,
            input: json!({"limit": 1}),
            locations: vec![],
        }),
        None,
    );

    assert!(matches!(
        rx.try_recv().expect("thinking start"),
        Event::ThinkingStart(_)
    ));
    assert!(matches!(
        rx.try_recv().expect("thinking text start"),
        Event::ThinkingTextMessageStart(_)
    ));
    match rx.try_recv().expect("thinking delta") {
        Event::ThinkingTextMessageContent(event) => assert_eq!(event.delta, "planning"),
        other => panic!("expected ThinkingTextMessageContent, got: {other:?}"),
    }
    assert!(matches!(
        rx.try_recv().expect("thinking text end"),
        Event::ThinkingTextMessageEnd(_)
    ));
    assert!(matches!(
        rx.try_recv().expect("thinking end"),
        Event::ThinkingEnd(_)
    ));
    match rx.try_recv().expect("tool start") {
        Event::ToolCallStart(event) => {
            assert_eq!(event.parent_message_id, Some(message_id));
            assert_eq!(event.tool_call_name, "read_history");
        }
        other => panic!("expected ToolCallStart, got: {other:?}"),
    }
    assert!(matches!(
        rx.try_recv().expect("tool args"),
        Event::ToolCallArgs(_)
    ));
    assert!(rx.try_recv().is_err());
}

#[test]
fn ag_ui_sink_keeps_thinking_open_across_background_session_event() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::with_snapshot(tx, message_id, false, None);

    sink.emit(
        AgentEvent::Model(ModelEvent::ThoughtChunk {
            blocks: vec![ContentBlock::Text("thinking one".to_string())],
        }),
        None,
    );
    sink.emit(
        AgentEvent::Session(SessionEvent::Saved {
            path: "/tmp/session.json".into(),
        }),
        None,
    );
    sink.emit(
        AgentEvent::Model(ModelEvent::ThoughtChunk {
            blocks: vec![ContentBlock::Text(" thinking two".to_string())],
        }),
        None,
    );
    sink.emit(
        AgentEvent::Turn(TurnEvent::Ended {
            outcome: harnx_core::event::TurnOutcome::default(),
        }),
        None,
    );

    assert!(matches!(
        rx.try_recv().expect("thinking start"),
        Event::ThinkingStart(_)
    ));
    assert!(matches!(
        rx.try_recv().expect("thinking text start"),
        Event::ThinkingTextMessageStart(_)
    ));
    match rx.try_recv().expect("first thinking delta") {
        Event::ThinkingTextMessageContent(event) => assert_eq!(event.delta, "thinking one"),
        other => panic!("expected ThinkingTextMessageContent, got: {other:?}"),
    }
    match rx.try_recv().expect("saved custom event") {
        Event::Custom(event) => {
            assert_eq!(event.name, "session_saved");
            assert_eq!(event.value, json!({ "path": "/tmp/session.json" }));
        }
        other => panic!("expected Custom, got: {other:?}"),
    }
    match rx.try_recv().expect("second thinking delta") {
        Event::ThinkingTextMessageContent(event) => assert_eq!(event.delta, " thinking two"),
        other => panic!("expected ThinkingTextMessageContent, got: {other:?}"),
    }
    assert!(matches!(
        rx.try_recv().expect("thinking text end"),
        Event::ThinkingTextMessageEnd(_)
    ));
    assert!(matches!(
        rx.try_recv().expect("thinking end"),
        Event::ThinkingEnd(_)
    ));
    assert!(matches!(
        rx.try_recv().expect("step finished"),
        Event::StepFinished(_)
    ));
    assert!(rx.try_recv().is_err());
}

#[test]
fn ag_ui_sink_maps_tool_started_completed_to_ag_ui_events() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::with_snapshot(tx, message_id.clone(), false, None);
    sink.emit(
        AgentEvent::Tool(ToolEvent::Started {
            id: "history-1".to_string(),
            name: "read_history".to_string(),
            kind: harnx_core::event::ToolKind::Other,
            markdown: None,
            input: json!({"limit": 5, "entry_type": "message"}),
            locations: vec![],
        }),
        None,
    );
    sink.emit(
        AgentEvent::Tool(ToolEvent::Completed {
            id: "history-1".to_string(),
            output: json!("history checked"),
            markdown: None,
        }),
        None,
    );

    match rx.try_recv().expect("tool start") {
        Event::ToolCallStart(event) => {
            assert_eq!(
                event.tool_call_id,
                serde_json::from_value(json!("history-1")).unwrap()
            );
            assert_eq!(event.tool_call_name, "read_history");
            assert_eq!(event.parent_message_id, Some(message_id.clone()));
        }
        other => panic!("expected ToolCallStart, got: {other:?}"),
    }

    match rx.try_recv().expect("tool args") {
        Event::ToolCallArgs(event) => {
            assert_eq!(
                event.tool_call_id,
                serde_json::from_value(json!("history-1")).unwrap()
            );
            assert_eq!(
                event.delta,
                json!({"limit": 5, "entry_type": "message"}).to_string()
            );
        }
        other => panic!("expected ToolCallArgs, got: {other:?}"),
    }

    match rx.try_recv().expect("tool end") {
        Event::ToolCallEnd(event) => {
            assert_eq!(
                event.tool_call_id,
                serde_json::from_value(json!("history-1")).unwrap()
            );
        }
        other => panic!("expected ToolCallEnd, got: {other:?}"),
    }

    match rx.try_recv().expect("tool result") {
        Event::ToolCallResult(event) => {
            assert_eq!(
                event.tool_call_id,
                serde_json::from_value(json!("history-1")).unwrap()
            );
            assert_eq!(event.content, "history checked");
            assert_eq!(event.role, Role::Tool);
        }
        other => panic!("expected ToolCallResult, got: {other:?}"),
    }

    assert!(rx.try_recv().is_err());
}

#[test]
fn ag_ui_sink_maps_tool_failures_and_blocked_to_results() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::new(tx, message_id);

    sink.emit(
        AgentEvent::Tool(ToolEvent::Failed {
            id: "tool-fail".to_string(),
            error: "boom".to_string(),
        }),
        None,
    );
    sink.emit(
        AgentEvent::Tool(ToolEvent::Blocked {
            id: "tool-blocked".to_string(),
            name: "danger".to_string(),
            input: json!({"path": "/tmp"}),
            reason: "blocked by policy".to_string(),
        }),
        None,
    );

    for (expected_id, expected_content) in
        [("tool-fail", "boom"), ("tool-blocked", "blocked by policy")]
    {
        match rx.try_recv().expect("tool end") {
            Event::ToolCallEnd(event) => {
                assert_eq!(
                    event.tool_call_id,
                    serde_json::from_value(json!(expected_id)).unwrap()
                );
            }
            other => panic!("expected ToolCallEnd, got: {other:?}"),
        }
        match rx.try_recv().expect("tool result") {
            Event::ToolCallResult(event) => {
                assert_eq!(
                    event.tool_call_id,
                    serde_json::from_value(json!(expected_id)).unwrap()
                );
                assert_eq!(event.content, expected_content);
                assert_eq!(event.role, Role::Tool);
            }
            other => panic!("expected ToolCallResult, got: {other:?}"),
        }
    }

    assert!(rx.try_recv().is_err());
}

#[test]
fn frame_event_uses_data_only_sse_format() {
    let event: Event = Event::RunStarted(RunStartedEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        thread_id: ThreadId::from(Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap()),
        run_id: RunId::from(Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap()),
    });

    let framed = frame_event(&event).expect("frame should serialize");
    assert!(framed.starts_with("data: {\"type\":\"RUN_STARTED\""));
    assert!(framed.ends_with("\n\n"));
    assert!(!framed.contains("event:"));
}

#[test]
fn frame_run_boundary_event_escapes_run_id_without_injecting_records() {
    let run_id = "client\"run\nnext-data:";
    let frame = frame_run_boundary_event("RUN_STARTED", "thread-1", run_id);
    assert_eq!(
        frame.matches("\ndata: ").count(),
        0,
        "must stay single SSE record"
    );
    let event = parse_sse_frame(frame.trim_end());
    assert_eq!(event["type"], "RUN_STARTED");
    assert_eq!(event["threadId"], "thread-1");
    assert_eq!(event["runId"], run_id);
}

#[test]
fn parse_run_input_accepts_valid_body() {
    let body = json!({
        "threadId": Uuid::new_v4(),
        "runId": Uuid::new_v4(),
        "state": {},
        "messages": [{
            "id": Uuid::new_v4(),
            "role": "user",
            "content": "hello"
        }],
        "tools": [],
        "context": [],
        "forwardedProps": {}
    });

    let parsed = parse_run_input(&serde_json::to_vec(&body).unwrap()).expect("valid body");
    assert_eq!(parsed.messages.len(), 1);
}

#[test]
fn ag_ui_sink_maps_turn_started_and_ended_to_step_events() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let sink = super::AgUiSink::with_snapshot(tx, MessageId::random(), true, None);

    sink.emit(AgentEvent::Turn(TurnEvent::Started), None);
    sink.emit(
        AgentEvent::Turn(TurnEvent::Ended {
            outcome: harnx_core::event::TurnOutcome::default(),
        }),
        None,
    );

    match rx.try_recv().expect("step started") {
        Event::StepStarted(event) => assert_eq!(event.step_name, "turn-1"),
        other => panic!("expected StepStarted, got: {other:?}"),
    }
    match rx.try_recv().expect("step finished") {
        Event::StepFinished(event) => assert_eq!(event.step_name, "turn-1"),
        other => panic!("expected StepFinished, got: {other:?}"),
    }
    assert!(rx.try_recv().is_err());
}

#[test]
fn ag_ui_sink_maps_plan_event_to_custom() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let sink = super::AgUiSink::with_snapshot(tx, MessageId::random(), true, None);

    sink.emit(
        AgentEvent::Plan {
            entries: vec![harnx_core::event::PlanEntry {
                status: "in_progress".to_string(),
                content: "check logs".to_string(),
            }],
        },
        None,
    );

    match rx.try_recv().expect("plan custom") {
        Event::Custom(event) => {
            assert_eq!(event.name, "plan");
            assert_eq!(
                event.value,
                json!([{"status": "in_progress", "content": "check logs"}])
            );
        }
        other => panic!("expected Custom plan, got: {other:?}"),
    }
}

#[test]
fn ag_ui_sink_maps_compaction_completed_to_custom_and_snapshot() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let snapshot = Arc::new(|| {
        vec![AgUiMessage::User {
            id: MessageId::random(),
            content: "compacted".to_string(),
            name: None,
        }]
    });
    let sink = super::AgUiSink::with_snapshot(tx, MessageId::random(), true, Some(snapshot));

    sink.emit(AgentEvent::Session(SessionEvent::CompactingCompleted), None);

    match rx.try_recv().expect("compaction custom") {
        Event::Custom(event) => {
            assert_eq!(event.name, "session_compacting_completed");
            assert_eq!(event.value, json!({}));
        }
        other => panic!("expected compaction custom, got: {other:?}"),
    }
    match rx.try_recv().expect("snapshot") {
        Event::MessagesSnapshot(event) => {
            assert_eq!(event.messages.len(), 1);
            assert!(
                matches!(&event.messages[0], AgUiMessage::User { content, .. } if content == "compacted")
            );
        }
        other => panic!("expected MessagesSnapshot, got: {other:?}"),
    }
}

#[test]
fn parse_run_input_preserves_tool_message_tool_call_id() {
    let body = json!({
        "threadId": Uuid::new_v4(),
        "runId": Uuid::new_v4(),
        "messages": [{
            "role": "tool",
            "toolCallId": "tool-call-123",
            "content": "tool output"
        }]
    });

    let parsed =
        parse_run_input(&serde_json::to_vec(&body).unwrap()).expect("tool message should parse");
    match &parsed.messages[0] {
        AgUiMessage::Tool { tool_call_id, .. } => {
            assert_eq!(tool_call_id.to_string(), "tool-call-123")
        }
        other => panic!("expected tool message, got {other:?}"),
    }
}

#[test]
fn parse_run_input_accepts_nanoid_ids() {
    let body = json!({
        "threadId": "thread_nanoid_123",
        "runId": "run_nanoid_456",
        "messages": [{
            "id": "msg_nanoid_789",
            "role": "user",
            "content": "hello"
        }]
    });

    let parsed =
        parse_run_input(&serde_json::to_vec(&body).unwrap()).expect("nanoid ids should parse");
    assert_eq!(parsed.messages.len(), 1);
}

#[test]
fn parse_run_input_accepts_minimal_body_without_optional_fields() {
    // Per plan guardrail: only `messages` is required; state/tools/context/forwardedProps
    // are semantically optional and should default if omitted.
    let thread_id = Uuid::new_v4();
    let run_id = Uuid::new_v4();
    let body = json!({
        "threadId": thread_id,
        "runId": run_id,
        "messages": [{
            "id": Uuid::new_v4(),
            "role": "user",
            "content": "hello"
        }]
    });

    let parsed = parse_run_input(&serde_json::to_vec(&body).unwrap()).expect("minimal body");
    assert_eq!(parsed.messages.len(), 1);
    assert_eq!(parsed.thread_id, ThreadId::from(thread_id));
    assert_eq!(parsed.run_id, RunId::from(run_id));
}

#[test]
fn parse_run_input_rejects_missing_messages_field() {
    let body = json!({
        "threadId": Uuid::new_v4(),
        "runId": Uuid::new_v4(),
        "state": {},
        "tools": [],
        "context": [],
        "forwardedProps": {}
    });

    let err = parse_run_input(&serde_json::to_vec(&body).unwrap())
        .expect_err("missing messages should fail");
    assert!(matches!(err, AgUiError::BadRequest(msg) if msg.contains("messages")));
}

#[test]
fn parse_run_input_accepts_empty_messages_for_join_resume() {
    let body = json!({
        "threadId": Uuid::new_v4(),
        "runId": Uuid::new_v4(),
        "state": {},
        "messages": [],
        "tools": [],
        "context": [],
        "forwardedProps": {}
    });

    let parsed = parse_run_input(&serde_json::to_vec(&body).unwrap())
        .expect("empty messages should be allowed for join/resume");
    assert!(parsed.messages.is_empty());
}

#[test]
fn resolve_agent_finds_existing_agent() {
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("hephaestus", "You are Hephaestus.");
    let config = sandbox.config();

    let agent = resolve_agent(&config, "hephaestus").expect("agent should resolve");
    assert_eq!(agent.name(), "hephaestus");
}

#[test]
fn resolve_agent_returns_not_found_for_unknown_agent() {
    let sandbox = TestConfigSandbox::new();
    let config = sandbox.config();

    let err = resolve_agent(&config, "missing-agent").expect_err("missing agent should fail");
    assert_eq!(
        err,
        AgUiError::NotFound("agent 'missing-agent' not found".to_string())
    );
}

#[test]
fn derive_thread_id_is_deterministic_and_passes_through_uuid() {
    let non_uuid = "not-a-uuid-session";
    let derived_a = derive_thread_id(non_uuid);
    let derived_b = derive_thread_id(non_uuid);
    assert_eq!(derived_a, derived_b);

    let session_uuid = Uuid::new_v4();
    let thread_id = derive_thread_id(&session_uuid.to_string());
    assert_eq!(thread_id, ThreadId::from(session_uuid));
}

#[test]
fn build_local_input_sets_agent_and_session_meta() {
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("hephaestus", "You are Hephaestus.");
    let base_config = sandbox.config();
    let prompt_config = harnx_session::fork_prompt_config(&base_config);
    {
        let mut config = prompt_config.write();
        config
            .use_agent_by_name("hephaestus")
            .expect("agent should load");
        config
            .use_session(Some("session-a"))
            .expect("session should load");
    }

    let input = build_local_input(&prompt_config, "hephaestus", "session-a", "hello world")
        .expect("input should build");
    assert!(input.with_session());
    assert!(input.with_agent());

    let session = prompt_config
        .read()
        .session
        .clone()
        .expect("session should exist");
    assert_eq!(session.agent_name.as_deref(), Some("hephaestus"));
}

#[tokio::test]
async fn ag_ui_run_streams_ordered_events_with_stubbed_call_fn() {
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("hephaestus", "You are Hephaestus.");
    let config = sandbox.config();

    let run_id_uuid = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
    let session_id = "run-session-1";
    let req_body = serde_json::to_vec(&json!({
        "threadId": Uuid::new_v4(),
        "runId": run_id_uuid,
        "state": {},
        "messages": [{
            "id": Uuid::new_v4(),
            "role": "user",
            "content": "forge response"
        }],
        "tools": [],
        "context": [],
        "forwardedProps": {}
    }))
    .unwrap();

    let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async move {
            harnx_core::sink::emit_agent_event(AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("chunk-text".to_string())],
            }));
            Ok((
                "assistant final".to_string(),
                None,
                Vec::<ToolCall>::new(),
                CompletionTokenUsage::default(),
            ))
        })
    });

    let registry = ag_ui_test_registry(&config, Some(call_fn));
    let response = ag_ui_run_with_call_fn(
        &config,
        &registry,
        "hephaestus",
        session_id,
        &req_body,
        None,
    )
    .await
    .expect("AG-UI run should succeed");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );

    let parsed_events = read_sse_events_until(response, |events| events.len() >= 6).await;

    let event_types = parsed_events
        .iter()
        .map(|event| event["type"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        event_types,
        vec![
            "RUN_STARTED",
            "MESSAGES_SNAPSHOT",
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_CONTENT",
            "TEXT_MESSAGE_END",
            "RUN_FINISHED",
        ]
    );
    assert_eq!(
        parsed_events[0]["runId"].as_str(),
        Some(run_id_uuid.to_string().as_str())
    );
    assert_eq!(
        parsed_events[5]["runId"].as_str(),
        Some(run_id_uuid.to_string().as_str())
    );

    assert_eq!(parsed_events[3]["delta"].as_str(), Some("chunk-text"));

    let start_message_id = parsed_events[2]["messageId"].as_str().unwrap().to_string();
    assert_eq!(
        parsed_events[3]["messageId"].as_str(),
        Some(start_message_id.as_str())
    );
    assert_eq!(
        parsed_events[3]["messageId"].as_str(),
        Some(start_message_id.as_str())
    );

    let session_messages = load_session_texts(&config, "hephaestus", session_id);
    assert!(
        session_messages.iter().any(|text| text == "forge response"),
        "user prompt should persist via loop"
    );
    assert!(
        session_messages
            .iter()
            .any(|text| text == "assistant final"),
        "assistant output should persist via loop"
    );
}

#[tokio::test]
async fn ag_ui_run_emits_run_error_without_run_finished_when_call_fn_fails() {
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let session_id = "error-path-session";
    let body = json!({
        "threadId": Uuid::new_v4(),
        "runId": Uuid::new_v4(),
        "messages": [{
            "id": Uuid::new_v4(),
            "role": "user",
            "content": "fail deterministically"
        }]
    });

    let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async { Err(anyhow!("stubbed call failure")) })
    });

    let registry = ag_ui_test_registry(&config, Some(call_fn));
    let response = ag_ui_run_with_call_fn(
        &config,
        &registry,
        "plain",
        session_id,
        &serde_json::to_vec(&body).unwrap(),
        None,
    )
    .await
    .expect("ag ui response");
    let parsed_events = read_sse_events_until(response, |events| {
        events
            .iter()
            .any(|event| event["type"].as_str() == Some("RUN_ERROR"))
    })
    .await;
    let event_types = parsed_events
        .iter()
        .map(|event| event["type"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        event_types,
        vec![
            "RUN_STARTED",
            "MESSAGES_SNAPSHOT",
            "TEXT_MESSAGE_START",
            "RUN_ERROR",
        ]
    );
    assert_eq!(
        parsed_events[3]["message"].as_str(),
        Some("stubbed call failure")
    );
    assert!(parsed_events
        .iter()
        .all(|event| event["type"].as_str() != Some("RUN_FINISHED")));
}

#[tokio::test]
async fn ag_ui_run_lists_persisted_session_and_history_for_agent() {
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let session_id = "enumeration-session";
    let body = json!({
        "threadId": Uuid::new_v4(),
        "runId": Uuid::new_v4(),
        "messages": [{
            "id": Uuid::new_v4(),
            "role": "user",
            "content": "enumerate persisted history"
        }]
    });

    let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async {
            let usage = CompletionTokenUsage::new(Some(1), Some(1), Some(0));
            Ok(("persisted assistant".into(), None, vec![], usage))
        })
    });

    let registry = ag_ui_test_registry(&config, Some(call_fn));
    let response = ag_ui_run_with_call_fn(
        &config,
        &registry,
        "plain",
        session_id,
        &serde_json::to_vec(&body).unwrap(),
        None,
    )
    .await
    .expect("persisted run response");
    assert_run_finished(
        read_sse_events_until(response, |events| {
            events
                .iter()
                .any(|event| event["type"].as_str() == Some("RUN_FINISHED"))
        })
        .await,
    );

    let scoped = super::super::agent_scoped_config(&config, "plain").expect("scoped config");
    let sessions = super::super::agent_sessions_json(&config, "plain").expect("session list");
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].get("session_id").is_some());

    let loaded =
        harnx_runtime::config::session::load(&scoped, session_id, &scoped.session_file(session_id))
            .expect("load persisted session");
    assert_eq!(loaded.agent_name.as_deref(), Some("plain"));

    let mut history_iter = loaded.messages.iter();
    let persisted_user = history_iter
        .find(|message| message.role.is_user())
        .expect("persisted user message");
    let persisted_assistant = history_iter
        .find(|message| message.role.is_assistant())
        .expect("persisted assistant message");
    assert_eq!(
        persisted_user.content.to_text(),
        "enumerate persisted history"
    );
    assert_eq!(persisted_assistant.content.to_text(), "persisted assistant");

    let shaped = loaded
        .messages
        .iter()
        .filter(|message| message.role.is_user() || message.role.is_assistant())
        .map(|message| {
            json!({
                "id": format!(
                    "seq:{}:0",
                    message.log_seq.expect("persisted history should have log seq")
                ),
                "role": super::super::history_role_name(message.role),
                "content": super::super::history_message_content(message),
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        shaped,
        vec![
            json!({
                "id": persisted_user
                    .log_seq
                    .map(|seq| format!("seq:{seq}:0"))
                    .expect("persisted user should have log seq"),
                "role": "user",
                "content": "enumerate persisted history"
            }),
            json!({
                "id": persisted_assistant
                    .log_seq
                    .map(|seq| format!("seq:{seq}:0"))
                    .expect("persisted assistant should have log seq"),
                "role": "assistant",
                "content": "persisted assistant"
            }),
        ]
    );

    let all_agents = Config::all_agents();
    assert!(all_agents.iter().any(|agent| agent.name() == "plain"));
}

#[tokio::test]
async fn ag_ui_run_uses_same_message_id_for_wire_and_persistence() {
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let session_id = "wire-id-session";
    let body = json!({
        "threadId": Uuid::new_v4(),
        "runId": Uuid::new_v4(),
        "messages": [{
            "id": Uuid::new_v4(),
            "role": "user",
            "content": "user1"
        }]
    });

    let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async {
            let usage = CompletionTokenUsage::new(Some(1), Some(1), Some(0));
            Ok(("assistant1".into(), None, vec![], usage))
        })
    });

    let registry = ag_ui_test_registry(&config, Some(call_fn));
    let response = ag_ui_run_with_call_fn(
        &config,
        &registry,
        "plain",
        session_id,
        &serde_json::to_vec(&body).unwrap(),
        None,
    )
    .await
    .expect("run response");
    let events = read_sse_events_until(response, |events| {
        events
            .iter()
            .any(|event| event["type"].as_str() == Some("RUN_FINISHED"))
    })
    .await;
    assert_run_finished(events.clone());

    let wire_message_ids = events
        .iter()
        .filter_map(|event| match event["type"].as_str() {
            Some("TEXT_MESSAGE_START")
            | Some("TEXT_MESSAGE_CONTENT")
            | Some("TEXT_MESSAGE_END") => event["messageId"].as_str().map(str::to_string),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !wire_message_ids.is_empty(),
        "expected wire message ids in SSE: {events:?}"
    );
    assert!(
        wire_message_ids.windows(2).all(|ids| ids[0] == ids[1]),
        "wire message ids should stay stable across SSE events: {wire_message_ids:?}"
    );
    let persisted_messages = load_session_messages(&config, "plain", session_id);
    let persisted_assistant = persisted_messages
        .iter()
        .find(|msg| msg.role.is_assistant())
        .expect("persisted assistant message");
    assert!(persisted_assistant.id.is_some());
}

fn assert_bad_request_contains(err: &AgUiError, expected: &str) {
    assert_eq!(err.status_code(), StatusCode::BAD_REQUEST);
    match err {
        AgUiError::BadRequest(message) => {
            assert!(
                message.contains(expected),
                "expected bad request to contain {expected:?}, got {message:?}"
            );
        }
        other => panic!("expected bad request error, got {other:?}"),
    }
}

#[tokio::test]
async fn ag_ui_run_idle_join_emits_keep_alive_comment() {
    tokio::time::pause();
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let registry = crate::session_actor::SessionRegistry::new(config.clone());
    let body = json!({
        "threadId": Uuid::new_v4(),
        "runId": Uuid::new_v4(),
        "messages": []
    });
    let response = ag_ui_run_with_call_fn(
        &config,
        &registry,
        "plain",
        "idle-keepalive-session",
        &serde_json::to_vec(&body).unwrap(),
        None,
    )
    .await
    .expect("idle join response");
    let read_task = tokio::spawn(async move {
        read_sse_until(
            response,
            SSE_KEEPALIVE_INTERVAL + Duration::from_secs(5),
            |read| !read.events.is_empty() && !read.comments.is_empty(),
        )
        .await
    });
    let mut frame_stream = tokio_stream::StreamExt::chain(
        tokio_stream::StreamExt::map(tokio_stream::once(snapshot_event(Vec::new())), |event| {
            let frame = frame_event(&event).expect("snapshot frame");
            Ok::<_, Infallible>(Bytes::from(frame))
        }),
        tokio_stream::StreamExt::map(
            keep_alive_stream(TEST_SSE_KEEPALIVE_INTERVAL),
            Ok::<_, Infallible>,
        ),
    );
    let snapshot = tokio::time::timeout(Duration::from_secs(1), frame_stream.next())
        .await
        .expect("snapshot timeout")
        .expect("snapshot frame")
        .expect("snapshot bytes");
    let keep_alive = tokio::time::timeout(Duration::from_secs(1), frame_stream.next())
        .await
        .expect("keep-alive timeout")
        .expect("keep-alive frame")
        .expect("keep-alive bytes");
    assert!(std::str::from_utf8(&snapshot)
        .expect("snapshot utf8")
        .starts_with("data: "));
    assert_eq!(
        std::str::from_utf8(&keep_alive).expect("keep-alive utf8"),
        keep_alive_frame()
    );
    tokio::task::yield_now().await;
    tokio::time::advance(SSE_KEEPALIVE_INTERVAL + Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    let read = read_task.await.expect("join read task");
    assert_event_type_sequence(
        &read.events,
        &["RUN_STARTED", "MESSAGES_SNAPSHOT", "RUN_FINISHED"],
    );
    assert!(read.comments.iter().any(|frame| frame == ": keep-alive"));
    assert!(
        !read
            .events
            .iter()
            .any(|event| event["type"] == ": keep-alive"),
        "keep-alive comment should not parse as AG-UI event: {:?}",
        read.events
    );
}

#[tokio::test]
async fn ag_ui_run_empty_messages_join_only_snapshot_no_new_run() {
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let session_id = "join-only-session";

    let first_body = json!({
        "threadId": Uuid::new_v4(),
        "runId": Uuid::new_v4(),
        "messages": [{
            "id": Uuid::new_v4(),
            "role": "user",
            "content": "user1"
        }]
    });
    let first_call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async {
            let usage = CompletionTokenUsage::new(Some(1), Some(1), Some(0));
            Ok(("assistant1".into(), None, vec![], usage))
        })
    });
    let registry = ag_ui_test_registry(&config, Some(first_call_fn));
    let first_response = ag_ui_run_with_call_fn(
        &config,
        &registry,
        "plain",
        session_id,
        &serde_json::to_vec(&first_body).unwrap(),
        None,
    )
    .await
    .expect("first run");
    let _ = read_sse_events_until(first_response, |events| {
        events
            .iter()
            .any(|event| event["type"].as_str() == Some("RUN_FINISHED"))
    })
    .await;

    let join_body = json!({
        "threadId": Uuid::new_v4(),
        "runId": Uuid::new_v4(),
        "messages": []
    });
    let response = ag_ui_run_with_call_fn(
        &config,
        &registry,
        "plain",
        session_id,
        &serde_json::to_vec(&join_body).unwrap(),
        None,
    )
    .await
    .expect("join only");
    let events = read_sse_events_until(response, |events| events.len() >= 3).await;
    assert_eq!(events[0]["type"], "RUN_STARTED");
    assert_eq!(events[1]["type"], "MESSAGES_SNAPSHOT");
    assert_eq!(events[2]["type"], "RUN_FINISHED");
}

#[tokio::test]
async fn ag_ui_run_promptless_join_returns_persisted_history_in_snapshot() {
    // Issue #959: promptless join must return MESSAGES_SNAPSHOT with prior history.
    // Previously Subscribe sent stale (empty) snapshot; now it refreshes first.
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let session_id = "promptless-history-session";

    // First run: user + assistant message to seed history
    let first_body = json!({
        "threadId": Uuid::new_v4(),
        "runId": Uuid::new_v4(),
        "messages": [{
            "id": Uuid::new_v4(),
            "role": "user",
            "content": "first user prompt"
        }]
    });
    let first_call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async {
            let usage = CompletionTokenUsage::new(Some(1), Some(1), Some(0));
            Ok(("first assistant reply".into(), None, vec![], usage))
        })
    });
    let registry = ag_ui_test_registry(&config, Some(first_call_fn));
    let first_response = ag_ui_run_with_call_fn(
        &config,
        &registry,
        "plain",
        session_id,
        &serde_json::to_vec(&first_body).unwrap(),
        None,
    )
    .await
    .expect("first run");
    let _ = read_sse_events_until(first_response, |events| {
        events
            .iter()
            .any(|event| event["type"].as_str() == Some("RUN_FINISHED"))
    })
    .await;

    // Promptless join: empty messages array
    let join_body = json!({
        "threadId": Uuid::new_v4(),
        "runId": Uuid::new_v4(),
        "messages": []
    });
    let response = ag_ui_run_with_call_fn(
        &config,
        &registry,
        "plain",
        session_id,
        &serde_json::to_vec(&join_body).unwrap(),
        None,
    )
    .await
    .expect("promptless join");
    let events = read_sse_events_until(response, |events| events.len() >= 3).await;

    // Expected sequence: RUN_STARTED → MESSAGES_SNAPSHOT (with prior messages) → RUN_FINISHED
    assert_eq!(events[0]["type"], "RUN_STARTED");
    assert_eq!(events[1]["type"], "MESSAGES_SNAPSHOT");
    assert_eq!(events[2]["type"], "RUN_FINISHED");

    // Verify snapshot contains the seeded history (at minimum user + assistant)
    let snapshot = &events[1]["messages"];
    assert!(
        snapshot.is_array(),
        "MESSAGES_SNAPSHOT should have messages array"
    );
    let messages = snapshot.as_array().expect("messages array");
    assert!(
        messages.len() >= 2,
        "snapshot should have at least user + assistant"
    );

    // Find user and assistant messages by role (history may include system/tool entries)
    let user_msg = messages
        .iter()
        .find(|m| m["role"] == "user")
        .expect("should find user message in snapshot");
    assert_eq!(
        user_msg["content"].as_str().unwrap_or(""),
        "first user prompt"
    );

    let assistant_msg = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("should find assistant message in snapshot");
    // Content may be null for empty-text assistant messages in snapshot
    let content = assistant_msg["content"].as_str().unwrap_or("");
    assert!(
        content == "first assistant reply"
            || content.is_empty()
            || assistant_msg["content"].is_null(),
        "assistant content should match or be empty: got {:?}",
        assistant_msg["content"]
    );

    // Synthetic RUN_FINISHED must NOT have 'result' key (per P1 spec)
    assert!(
        events[2].get("result").is_none(),
        "promptless synthetic RUN_FINISHED must not have result key"
    );
}

#[tokio::test]
async fn ag_ui_run_uses_only_last_message_user_prompt() {
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let session_id = "last-message-session";

    let body = json!({
        "threadId": Uuid::new_v4(),
        "runId": Uuid::new_v4(),
        "messages": [
            {
                "id": Uuid::new_v4(),
                "role": "user",
                "content": "ignored earlier"
            },
            {
                "id": Uuid::new_v4(),
                "role": "assistant",
                "content": "assistant history"
            },
            {
                "id": Uuid::new_v4(),
                "role": "user",
                "content": "last user wins"
            }
        ]
    });
    let call_fn: AgentCallFn = Arc::new(|input, _config, _abort| {
        let text = input.text();
        Box::pin(async move {
            assert_eq!(text, "last user wins");
            let usage = CompletionTokenUsage::new(Some(1), Some(1), Some(0));
            Ok(("assistant2".into(), None, vec![], usage))
        })
    });
    let registry = ag_ui_test_registry(&config, Some(call_fn));
    let response = ag_ui_run_with_call_fn(
        &config,
        &registry,
        "plain",
        session_id,
        &serde_json::to_vec(&body).unwrap(),
        None,
    )
    .await
    .expect("last message prompt run");
    let events = read_sse_events_until(response, |events| {
        events.iter().any(|event| event["type"] == "RUN_FINISHED")
    })
    .await;
    assert_eq!(events[0]["type"], "RUN_STARTED");
    assert_eq!(events[1]["type"], "MESSAGES_SNAPSHOT");
    assert!(events.iter().any(|event| event["type"] == "RUN_STARTED"));
}

#[tokio::test]
async fn ag_ui_run_echoes_client_nanoid_run_id_in_boundary_events() {
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let session_id = "echo-run-id-session";
    let client_run_id = "client_run_nanoid_123";
    let body = json!({
        "threadId": "client_thread_nanoid_456",
        "runId": client_run_id,
        "messages": [{
            "id": "client_msg_nanoid_789",
            "role": "user",
            "content": "echo run id"
        }]
    });
    let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async {
            let usage = CompletionTokenUsage::new(Some(1), Some(1), Some(0));
            Ok(("assistant".into(), None, vec![], usage))
        })
    });
    let registry = ag_ui_test_registry(&config, Some(call_fn));
    let response = ag_ui_run_with_call_fn(
        &config,
        &registry,
        "plain",
        session_id,
        &serde_json::to_vec(&body).unwrap(),
        None,
    )
    .await
    .expect("run response");
    let events = read_sse_events_until(response, |events| events.len() >= 3).await;
    assert_eq!(events[0]["type"], "RUN_STARTED");
    assert_eq!(events[0]["runId"].as_str(), Some(client_run_id));
    assert!(
        events
            .iter()
            .all(|event| event["runId"].as_str() != Some(client_run_id)
                || event["type"] == "RUN_STARTED"
                || event["type"] == "RUN_FINISHED"),
        "client run id should only appear on boundary events: {events:?}"
    );
}

#[tokio::test]
async fn ag_ui_run_resume_same_session_persists_only_new_turn() {
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let session_id = "resume-session";

    let first_body = json!({
        "threadId": Uuid::new_v4(),
        "runId": Uuid::new_v4(),
        "messages": [{
            "id": Uuid::new_v4(),
            "role": "user",
            "content": "user1"
        }]
    });
    let first_call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async {
            let usage = CompletionTokenUsage::new(Some(1), Some(1), Some(0));
            Ok(("assistant1".into(), None, vec![], usage))
        })
    });
    let registry = ag_ui_test_registry(&config, Some(first_call_fn));
    let first_response = ag_ui_run_with_call_fn(
        &config,
        &registry,
        "plain",
        session_id,
        &serde_json::to_vec(&first_body).unwrap(),
        None,
    )
    .await
    .expect("first run");
    assert_run_finished(
        read_sse_events_until(first_response, |events| {
            events
                .iter()
                .any(|event| event["type"].as_str() == Some("RUN_FINISHED"))
        })
        .await,
    );
    let first_messages = load_session_texts(&config, "plain", session_id);
    assert_eq!(
        first_messages,
        vec!["user1".to_string(), "assistant1".to_string()]
    );

    let persisted_messages = load_session_messages(&config, "plain", session_id);
    let persisted_assistant_id = persisted_messages
        .iter()
        .find(|msg| msg.role.is_assistant())
        .and_then(|msg| msg.id.clone())
        .expect("persisted assistant id");

    let second_body = json!({
        "threadId": Uuid::new_v4(),
        "runId": Uuid::new_v4(),
        "messages": [
            {
                "id": MessageId::random(),
                "role": "user",
                "content": "user1"
            },
            {
                "id": persisted_assistant_id,
                "role": "assistant",
                "content": "assistant1"
            },
            {
                "id": MessageId::random(),
                "role": "user",
                "content": "user2"
            }
        ]
    });
    let second_call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async {
            let usage = CompletionTokenUsage::new(Some(1), Some(1), Some(0));
            Ok(("assistant2".into(), None, vec![], usage))
        })
    });
    let registry = ag_ui_test_registry(&config, Some(second_call_fn));
    let second_response = ag_ui_run_with_call_fn(
        &config,
        &registry,
        "plain",
        session_id,
        &serde_json::to_vec(&second_body).unwrap(),
        None,
    )
    .await
    .expect("second run");
    assert_run_finished(
        read_sse_events_until(second_response, |events| {
            events
                .iter()
                .any(|event| event["type"].as_str() == Some("RUN_FINISHED"))
        })
        .await,
    );
    let second_messages = load_session_texts(&config, "plain", session_id);
    assert_eq!(second_messages.len(), first_messages.len() + 2);
    assert_eq!(
        second_messages,
        vec![
            "user1".to_string(),
            "assistant1".to_string(),
            "user2".to_string(),
            "assistant2".to_string(),
        ]
    );
}

fn parse_sse_frames(body: &str) -> Vec<String> {
    body.split("\n\n")
        .filter_map(|frame| {
            let trimmed = frame.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .collect()
}

async fn parse_sse_events(response: AppResponse) -> Vec<Value> {
    let body = response
        .into_body()
        .collect()
        .await
        .expect("collect sse body")
        .to_bytes();
    let body_text = std::str::from_utf8(&body).expect("sse utf8");
    parse_sse_frames(body_text)
        .into_iter()
        .map(|frame| parse_sse_frame(&frame))
        .collect()
}

async fn read_sse_events_until<F>(response: AppResponse, done: F) -> Vec<Value>
where
    F: Fn(&[Value]) -> bool,
{
    read_sse_until(response, std::time::Duration::from_secs(5), |read| {
        done(&read.events)
    })
    .await
    .events
}

async fn read_sse_until<F>(response: AppResponse, timeout: Duration, done: F) -> SseRead
where
    F: Fn(&SseRead) -> bool,
{
    let mut body = response.into_body().into_data_stream();
    let mut read = SseRead::default();
    let mut partial = String::new();

    while !done(&read) {
        let next = tokio::time::timeout(timeout, tokio_stream::StreamExt::next(&mut body))
            .await
            .expect("timed out waiting for SSE chunk");
        let chunk = next
            .expect("sse stream ended before expected frame")
            .expect("stream chunk");
        partial.push_str(std::str::from_utf8(&chunk).expect("sse utf8"));

        while let Some(idx) = partial.find("\n\n") {
            let frame = partial[..idx].trim().to_string();
            partial.drain(..idx + 2);
            if frame.is_empty() {
                continue;
            }
            read.frames.push(frame.clone());
            if frame.starts_with(':') {
                read.comments.push(frame);
            } else {
                read.events.push(parse_sse_frame(&frame));
            }
            if done(&read) {
                return read;
            }
        }
    }

    read
}

#[derive(Debug, Default)]
struct SseRead {
    frames: Vec<String>,
    events: Vec<Value>,
    comments: Vec<String>,
}

fn parse_sse_frame(frame: &str) -> Value {
    let payload = frame
        .strip_prefix("data: ")
        .expect("sse frame should start with data prefix");
    serde_json::from_str(payload).expect("frame should be valid json")
}

fn assert_run_finished(events: Vec<Value>) {
    assert!(
        events
            .iter()
            .any(|event| event["type"].as_str() == Some("RUN_FINISHED")),
        "expected RUN_FINISHED barrier, got events: {events:?}"
    );
}

fn load_session_texts(config: &Config, agent: &str, session_id: &str) -> Vec<String> {
    let prompt_config = harnx_session::fork_prompt_config(config);
    {
        let mut cfg = prompt_config.write();
        cfg.use_agent_by_name(agent).expect("set agent");
        cfg.use_session(Some(session_id)).expect("load session");
    }
    let session_messages = prompt_config
        .read()
        .session
        .as_ref()
        .expect("session should exist")
        .messages
        .iter()
        .filter(|msg| msg.role.is_user() || msg.role.is_assistant())
        .map(|msg| msg.content.to_text())
        .collect();
    session_messages
}
fn load_session_messages(config: &Config, agent: &str, session_id: &str) -> Vec<HistoryMsg> {
    let prompt_config = harnx_session::fork_prompt_config(config);
    {
        let mut cfg = prompt_config.write();
        cfg.use_agent_by_name(agent).expect("set agent");
        cfg.use_session(Some(session_id)).expect("load session");
    }
    let messages = prompt_config
        .read()
        .session
        .as_ref()
        .expect("session should exist")
        .messages
        .iter()
        .filter(|msg| msg.role.is_user() || msg.role.is_assistant())
        .cloned()
        .collect();
    messages
}

#[test]
fn history_messages_for_snapshot_keeps_tool_turn_prose_and_tool_entries() {
    let tool_call = harnx_core::tool::ToolCall::new(
        "history".into(),
        json!({"limit": 5}),
        Some("call-1".into()),
        None,
    );
    let history = vec![
        HistoryMsg {
            role: MessageRole::User,
            content: MessageContent::Text("prompt".to_string()),
            id: Some(Uuid::new_v4().to_string()),
            log_seq: None,
            log_timestamp: None,
        },
        HistoryMsg {
            role: MessageRole::Assistant,
            content: MessageContent::ToolCalls(MessageContentToolCalls::new(
                vec![harnx_core::tool::ToolResult::new(
                    tool_call,
                    json!("tool output"),
                )],
                "assistant prose".to_string(),
                None,
            )),
            id: Some(Uuid::new_v4().to_string()),
            log_seq: None,
            log_timestamp: None,
        },
    ];

    let snapshot = history_messages_for_snapshot(&history);
    assert_eq!(snapshot.len(), 3);
    assert!(matches!(&snapshot[0], AgUiMessage::User { content, .. } if content == "prompt"));
    assert!(
        matches!(&snapshot[1], AgUiMessage::Assistant { content, .. } if content.as_deref() == Some("assistant prose"))
    );
    assert!(
        matches!(&snapshot[2], AgUiMessage::Tool { content, .. } if content.contains("tool output"))
    );
}

fn user_msg(content: &str) -> AgUiMessage {
    AgUiMessage::User {
        id: MessageId::random(),
        content: content.to_string(),
        name: None,
    }
}

fn assistant_msg(content: &str) -> AgUiMessage {
    AgUiMessage::Assistant {
        id: MessageId::random(),
        content: Some(content.to_string()),
        name: None,
        tool_calls: None,
    }
}
