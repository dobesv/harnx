use super::*;
use crate::test_support::TestConfigSandbox;
use anyhow::anyhow;
use harnx_core::{
    api_types::CompletionTokenUsage,
    event::{AgentEventSink, ModelEvent, SessionEvent, ToolEvent, TurnEvent},
    message::{Message, MessageContent, MessageContentToolCalls},
};
use harnx_runtime::{client::ToolCall, config::Config};
use http_body_util::BodyExt;
use serde_json::{json, Value};

#[path = "ag_ui_handoff_tests.rs"]
mod handoff_tests;

fn collect_events(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

fn assert_handoff_payload(event: &CustomEvent) {
    assert_eq!(event.value["agent"].as_str(), Some("target-agent"));
    assert_eq!(
        event.value["session_id"].as_str(),
        Some("target-session-123")
    );
}

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

    sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
        blocks: vec![ContentBlock::Text("hello".to_string())],
    }));

    let opened_message_id = match rx.try_recv().expect("text start") {
        Event::TextMessageStart(event) => {
            assert_eq!(event.role, Role::Assistant);
            event.message_id
        }
        other => panic!("expected TextMessageStart event, got: {other:?}"),
    };
    match rx.try_recv().expect("text content") {
        Event::TextMessageContent(TextMessageContentEvent {
            base,
            message_id: mid,
            delta,
        }) => {
            assert_eq!(base.timestamp, None);
            assert_eq!(base.raw_event, None);
            assert_eq!(mid, opened_message_id);
            assert_eq!(delta, "hello");
        }
        other => panic!("expected TextMessageContent event, got: {other:?}"),
    }

    assert!(rx.try_recv().is_err());
}

#[test]
fn ag_ui_sink_unwraps_sub_agent_message_chunk() {
    // SubAgent wrapper is transparently unwrapped at the top of emit.
    // The resulting AG-UI events should match the bare MessageChunk path.
    use harnx_core::event::AgentSource;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id =
        MessageId::from(uuid::Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap());
    let sink = super::AgUiSink::with_snapshot(tx, message_id.clone(), false, None);

    let source = AgentSource {
        agent: "sub".into(),
        session_id: None,
        model: None,
    };
    sink.emit(AgentEvent::sub_agent(
        source,
        AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("hello".into())],
        }),
    ));

    let opened_message_id = match rx.try_recv().expect("text start") {
        Event::TextMessageStart(event) => {
            assert_eq!(event.role, Role::Assistant);
            event.message_id
        }
        other => panic!("expected TextMessageStart event, got: {other:?}"),
    };
    match rx.try_recv().expect("text content") {
        Event::TextMessageContent(TextMessageContentEvent {
            base,
            message_id: mid,
            delta,
        }) => {
            assert_eq!(base.timestamp, None);
            assert_eq!(base.raw_event, None);
            assert_eq!(mid, opened_message_id);
            assert_eq!(delta, "hello");
        }
        other => panic!("expected TextMessageContent event, got: {other:?}"),
    }

    assert!(rx.try_recv().is_err(), "no additional events expected");
}

#[test]
fn ag_ui_sink_does_not_promote_sub_agent_error_to_run_error() {
    // A nested agent failure belongs to that sub-agent step. It must not end
    // the parent AG-UI run.
    use harnx_core::event::AgentSource;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id =
        MessageId::from(uuid::Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap());
    let sink = super::AgUiSink::new(tx, message_id.clone());

    let source = AgentSource {
        agent: "sub".into(),
        session_id: None,
        model: None,
    };
    sink.emit(AgentEvent::sub_agent(
        source,
        AgentEvent::Model(ModelEvent::Error("boom".into())),
    ));

    assert!(rx.try_recv().is_err(), "no AG-UI run error expected");
    assert!(sink.take_run_error().is_none());
}

#[test]
fn ag_ui_sink_skips_empty_chunk() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::new(tx, message_id);

    sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
        blocks: vec![],
    }));
    assert!(rx.try_recv().is_err());

    sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
        blocks: vec![ContentBlock::Image {
            data: vec![],
            mime: "image/png".to_string(),
        }],
    }));
    assert!(rx.try_recv().is_err());
}

#[test]
fn ag_ui_sink_defers_model_error_to_run_owner() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::new(tx, message_id);

    sink.emit(AgentEvent::Model(ModelEvent::Error("boom".to_string())));

    assert!(rx.try_recv().is_err());
    assert_eq!(sink.take_run_error().as_deref(), Some("boom"));
    assert!(sink.take_run_error().is_none());
}

#[test]
fn ag_ui_sink_emits_custom_for_title_generation_failed() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::new(tx, message_id);

    sink.emit(AgentEvent::Session(SessionEvent::TitleGenerationFailed(
        "Miss 'api_key'".to_string(),
    )));

    let event = rx.try_recv().expect("should receive event");
    match event {
        Event::Custom(CustomEvent { name, value, .. }) => {
            assert_eq!(name, "session_title_generation_failed");
            assert_eq!(value["error"], json!("Miss 'api_key'"));
        }
        _ => panic!("expected Custom event, got: {:?}", event),
    }
}

#[test]
fn ag_ui_sink_emits_advisory_notices_without_failing_run() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::new(tx, message_id);

    sink.emit(AgentEvent::Notice(NoticeEvent::Warning(
        "partial stream failed".to_string(),
    )));

    let Event::Custom(CustomEvent { name, value, .. }) =
        rx.try_recv().expect("notice should be forwarded")
    else {
        panic!("expected custom notice event");
    };
    assert_eq!(name, "notice");
    assert_eq!(value["level"], json!("warning"));
    assert_eq!(value["message"], json!("partial stream failed"));
    assert!(sink.take_run_error().is_none());
}

#[test]
fn ag_ui_sink_emits_custom_status_event() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::new(tx, message_id);

    sink.emit(AgentEvent::Status(harnx_core::event::StatusLine {
        text: "working...".to_string(),
    }));

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
    sink.emit(AgentEvent::Model(ModelEvent::Final {
        output: "final text".to_string(),
        usage,
    }));

    let opened_message_id = match rx.try_recv().expect("text start") {
        Event::TextMessageStart(event) => {
            assert_eq!(event.role, Role::Assistant);
            event.message_id
        }
        other => panic!("expected TextMessageStart event, got: {other:?}"),
    };
    match rx.try_recv().expect("text content") {
        Event::TextMessageContent(TextMessageContentEvent {
            base,
            message_id: mid,
            delta,
        }) => {
            assert_eq!(mid, opened_message_id);
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
    sink.emit(AgentEvent::Model(ModelEvent::Final {
        output: String::new(),
        usage,
    }));

    assert!(rx.try_recv().is_err());
}

#[test]
fn ag_ui_sink_does_not_repeat_streamed_text_on_final() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::with_snapshot(tx, message_id.clone(), true, None);

    sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
        blocks: vec![ContentBlock::Text("answer".to_string())],
    }));
    sink.emit(AgentEvent::Model(ModelEvent::Final {
        output: "answer".to_string(),
        usage: CompletionTokenUsage::default(),
    }));

    match rx.try_recv().expect("streamed text") {
        Event::TextMessageContent(event) => {
            assert_eq!(event.message_id, message_id);
            assert_eq!(event.delta, "answer");
        }
        other => panic!("expected TextMessageContent, got: {other:?}"),
    }
    assert!(
        rx.try_recv().is_err(),
        "Final must not repeat streamed text"
    );
}

#[test]
fn ag_ui_sink_maps_thought_then_text_and_closes_thinking_before_text() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::with_snapshot(tx, message_id.clone(), false, None);

    sink.emit(AgentEvent::Model(ModelEvent::ThoughtChunk {
        blocks: vec![ContentBlock::Text("thinking...".to_string())],
    }));
    sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
        blocks: vec![ContentBlock::Text("answer".to_string())],
    }));
    sink.emit(AgentEvent::Model(ModelEvent::Final {
        output: String::new(),
        usage: CompletionTokenUsage::default(),
    }));

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
    let opened_message_id = match rx.try_recv().expect("text start") {
        Event::TextMessageStart(event) => {
            assert_eq!(event.role, Role::Assistant);
            event.message_id
        }
        other => panic!("expected TextMessageStart, got: {other:?}"),
    };
    match rx.try_recv().expect("text delta") {
        Event::TextMessageContent(event) => {
            assert_eq!(event.message_id, opened_message_id);
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

    sink.emit(AgentEvent::Model(ModelEvent::ThoughtChunk {
        blocks: vec![ContentBlock::Text("planning".to_string())],
    }));
    sink.emit(AgentEvent::Tool(ToolEvent::Started {
        id: "tool-1".to_string(),
        name: "read_history".to_string(),
        kind: harnx_core::event::ToolKind::Other,
        markdown: None,
        input: json!({"limit": 1}),
        locations: vec![],
    }));

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

    sink.emit(AgentEvent::Model(ModelEvent::ThoughtChunk {
        blocks: vec![ContentBlock::Text("thinking one".to_string())],
    }));
    sink.emit(AgentEvent::Session(SessionEvent::Saved {
        path: "/tmp/session.json".into(),
    }));
    sink.emit(AgentEvent::Model(ModelEvent::ThoughtChunk {
        blocks: vec![ContentBlock::Text(" thinking two".to_string())],
    }));
    sink.emit(AgentEvent::Turn(TurnEvent::Ended {
        outcome: harnx_core::event::TurnOutcome::default(),
    }));

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
    assert!(rx.try_recv().is_err());
}

#[test]
fn ag_ui_sink_maps_tool_started_completed_to_ag_ui_events() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::with_snapshot(tx, message_id.clone(), false, None);
    sink.emit(AgentEvent::Tool(ToolEvent::Started {
        id: "history-1".to_string(),
        name: "read_history".to_string(),
        kind: harnx_core::event::ToolKind::Other,
        markdown: None,
        input: json!({"limit": 5, "entry_type": "message"}),
        locations: vec![],
    }));
    sink.emit(AgentEvent::Tool(ToolEvent::Completed {
        id: "history-1".to_string(),
        output: json!("history checked"),
        markdown: None,
    }));

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
fn ag_ui_sink_segments_text_around_tool_calls() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::with_snapshot(tx, message_id.clone(), false, None);

    sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
        blocks: vec![ContentBlock::Text("A".to_string())],
    }));
    sink.emit(AgentEvent::Tool(ToolEvent::Started {
        id: "history-3".to_string(),
        name: "read_history".to_string(),
        kind: harnx_core::event::ToolKind::Other,
        markdown: None,
        input: json!({"limit": 1}),
        locations: vec![],
    }));
    sink.emit(AgentEvent::Tool(ToolEvent::Completed {
        id: "history-3".to_string(),
        output: json!("done"),
        markdown: None,
    }));
    sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
        blocks: vec![ContentBlock::Text("B".to_string())],
    }));

    let first_id = match rx.try_recv().expect("first text start") {
        Event::TextMessageStart(event) => {
            assert_eq!(event.role, Role::Assistant);
            event.message_id
        }
        other => panic!("expected first TextMessageStart, got: {other:?}"),
    };
    match rx.try_recv().expect("first text content") {
        Event::TextMessageContent(event) => {
            assert_eq!(event.message_id, first_id);
            assert_eq!(event.delta, "A");
        }
        other => panic!("expected first TextMessageContent, got: {other:?}"),
    }
    match rx.try_recv().expect("first text end") {
        Event::TextMessageEnd(event) => assert_eq!(event.message_id, first_id),
        other => panic!("expected first TextMessageEnd, got: {other:?}"),
    }
    match rx.try_recv().expect("tool start") {
        Event::ToolCallStart(event) => {
            assert_eq!(event.parent_message_id, Some(message_id.clone()));
            assert_eq!(
                event.tool_call_id,
                serde_json::from_value(json!("history-3")).unwrap()
            );
        }
        other => panic!("expected ToolCallStart, got: {other:?}"),
    }
    assert!(matches!(
        rx.try_recv().expect("tool args"),
        Event::ToolCallArgs(_)
    ));
    assert!(matches!(
        rx.try_recv().expect("tool end"),
        Event::ToolCallEnd(_)
    ));
    assert!(matches!(
        rx.try_recv().expect("tool result"),
        Event::ToolCallResult(_)
    ));
    let second_id = match rx.try_recv().expect("second text start") {
        Event::TextMessageStart(event) => {
            assert_eq!(event.role, Role::Assistant);
            event.message_id
        }
        other => panic!("expected second TextMessageStart, got: {other:?}"),
    };
    assert_ne!(first_id, second_id);
    match rx.try_recv().expect("second text content") {
        Event::TextMessageContent(event) => {
            assert_eq!(event.message_id, second_id);
            assert_eq!(event.delta, "B");
        }
        other => panic!("expected second TextMessageContent, got: {other:?}"),
    }
    match sink.close_text_segment() {
        Some(closed_id) => assert_eq!(closed_id, second_id),
        None => panic!("expected open text segment to close"),
    }
    match rx.try_recv().expect("second text end") {
        Event::TextMessageEnd(event) => assert_eq!(event.message_id, second_id),
        other => panic!("expected second TextMessageEnd, got: {other:?}"),
    }
    assert!(rx.try_recv().is_err());
}

#[test]
fn ag_ui_sink_does_not_emit_orphan_text_end_after_tool_only_tail() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let sink = super::AgUiSink::with_snapshot(tx, MessageId::random(), false, None);

    sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
        blocks: vec![ContentBlock::Text("A".to_string())],
    }));
    sink.emit(AgentEvent::Tool(ToolEvent::Started {
        id: "history-4".to_string(),
        name: "read_history".to_string(),
        kind: harnx_core::event::ToolKind::Other,
        markdown: None,
        input: json!({"limit": 1}),
        locations: vec![],
    }));

    while let Ok(_event) = rx.try_recv() {}
    assert!(sink.close_text_segment().is_none());
    assert!(rx.try_recv().is_err());
}

#[test]
fn ag_ui_sink_emits_tool_summary_custom_event_and_preserves_start_args() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::with_snapshot(tx, message_id.clone(), false, None);

    sink.emit(AgentEvent::Tool(ToolEvent::Started {
        id: "history-2".to_string(),
        name: "read_history".to_string(),
        kind: harnx_core::event::ToolKind::Other,
        markdown: Some("### Summary\n- item".to_string()),
        input: json!({"limit": 2}),
        locations: vec![],
    }));

    match rx.try_recv().expect("tool start") {
        Event::ToolCallStart(event) => {
            assert_eq!(
                event.tool_call_id,
                serde_json::from_value(json!("history-2")).unwrap()
            );
            assert_eq!(event.tool_call_name, "read_history");
            assert_eq!(event.parent_message_id, Some(message_id.clone()));
        }
        other => panic!("expected ToolCallStart, got: {other:?}"),
    }

    match rx.try_recv().expect("tool summary") {
        Event::Custom(CustomEvent { name, value, .. }) => {
            assert_eq!(name, "tool_summary");
            assert_eq!(
                value,
                json!({
                    "tool_call_id": "history-2",
                    "markdown": "### Summary\n- item"
                })
            );
        }
        other => panic!("expected tool_summary custom event, got: {other:?}"),
    }

    match rx.try_recv().expect("tool args") {
        Event::ToolCallArgs(event) => {
            assert_eq!(
                event.tool_call_id,
                serde_json::from_value(json!("history-2")).unwrap()
            );
            assert_eq!(event.delta, json!({"limit": 2}).to_string());
        }
        other => panic!("expected ToolCallArgs, got: {other:?}"),
    }

    assert!(rx.try_recv().is_err());
}

#[test]
fn ag_ui_sink_usage_event_includes_context_fields_and_legacy_fields() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let context = Arc::new(|| {
        Some(super::UsageContextSnapshot {
            context_tokens: 321,
            max_context_tokens: Some(1000),
            context_percent: Some(32.1),
        })
    });
    let sink =
        super::AgUiSink::with_snapshot_and_context(tx, message_id, true, None, Some(context));

    sink.emit(AgentEvent::Model(ModelEvent::Usage {
        input: 12,
        output: 34,
        cached: 5,
        session_label: Some("sess-a".to_string()),
    }));

    match rx.try_recv().expect("usage event") {
        Event::Custom(CustomEvent { name, value, .. }) => {
            assert_eq!(name, "usage");
            assert_eq!(value["input"], json!(12));
            assert_eq!(value["output"], json!(34));
            assert_eq!(value["cached"], json!(5));
            assert_eq!(value["session_label"], json!("sess-a"));
            assert_eq!(value["context_tokens"], json!(321));
            assert_eq!(value["max_context_tokens"], json!(1000));
            let pct = value["context_percent"]
                .as_f64()
                .expect("context_percent is a number");
            assert!((pct - 32.1).abs() < 0.01, "context_percent was {pct}");
        }
        other => panic!("expected usage custom event, got: {other:?}"),
    }

    assert!(rx.try_recv().is_err());
}

#[test]
fn ag_ui_sink_maps_tool_failures_and_blocked_to_results() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let message_id = MessageId::from(uuid::Uuid::new_v4());
    let sink = super::AgUiSink::new(tx, message_id);

    sink.emit(AgentEvent::Tool(ToolEvent::Failed {
        id: "tool-fail".to_string(),
        error: "boom".to_string(),
    }));
    sink.emit(AgentEvent::Tool(ToolEvent::Blocked {
        id: "tool-blocked".to_string(),
        name: "danger".to_string(),
        input: json!({"path": "/tmp"}),
        reason: "blocked by policy".to_string(),
    }));

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
fn ag_ui_sink_maps_only_sub_agent_turns_to_step_events() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let sink = super::AgUiSink::with_snapshot(tx, MessageId::random(), true, None);

    sink.emit(AgentEvent::Turn(TurnEvent::Started));
    sink.emit(AgentEvent::Turn(TurnEvent::Ended {
        outcome: harnx_core::event::TurnOutcome::default(),
    }));

    assert!(
        rx.try_recv().is_err(),
        "the parent turn is already represented by the AG-UI run"
    );

    let source = harnx_core::event::AgentSource {
        agent: "sub".into(),
        session_id: None,
        model: None,
    };
    sink.emit(AgentEvent::sub_agent(
        source.clone(),
        AgentEvent::Turn(TurnEvent::Started),
    ));
    sink.emit(AgentEvent::sub_agent(
        source,
        AgentEvent::Turn(TurnEvent::Ended {
            outcome: harnx_core::event::TurnOutcome::default(),
        }),
    ));

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

    sink.emit(AgentEvent::Plan {
        entries: vec![harnx_core::event::PlanEntry {
            status: "in_progress".to_string(),
            content: "check logs".to_string(),
        }],
    });

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

    sink.emit(AgentEvent::Session(SessionEvent::CompactingCompleted));

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
    let parsed_again =
        parse_run_input(&serde_json::to_vec(&body).unwrap()).expect("nanoid ids should reparse");
    assert_eq!(
        parsed.messages[0].id(),
        parsed_again.messages[0].id(),
        "non-UUID message IDs must map to a stable AG-UI UUID"
    );
    assert_eq!(
        pending_user_prompt(&parsed_again, &parsed.messages),
        None,
        "the stable wire ID must make nanoid-backed hydration idempotent"
    );
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
    let non_uuid = "aok2Gw";
    let derived_a = derive_thread_id(non_uuid);
    let derived_b = derive_thread_id(non_uuid);
    assert_eq!(derived_a, derived_b);
    assert!(Uuid::parse_str(&derived_a.to_string()).is_ok());

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

    let parsed_events = read_sse_events_until(response, |events| {
        events.iter().any(|event| event["type"] == "RUN_FINISHED")
    })
    .await;

    let event_types = parsed_events
        .iter()
        .map(|event| event["type"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    // A prompted run is a pure delta stream: no MESSAGES_SNAPSHOT (that would wipe
    // the client's optimistically-appended user message). Hydration happens on the
    // separate promptless subscribe stream instead.
    assert_eq!(
        event_types,
        vec![
            "RUN_STARTED",
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
        parsed_events[4]["runId"].as_str(),
        Some(run_id_uuid.to_string().as_str())
    );

    assert_eq!(parsed_events[2]["delta"].as_str(), Some("chunk-text"));

    let text_message_id = parsed_events[1]["messageId"].as_str().unwrap().to_string();
    assert_eq!(
        parsed_events[2]["messageId"].as_str(),
        Some(text_message_id.as_str())
    );
    // TEXT_MESSAGE_END (next frame) must carry same messageId as streamed content.
    assert_eq!(
        parsed_events[3]["messageId"].as_str(),
        Some(text_message_id.as_str())
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
async fn ag_ui_prompted_run_stream_terminates_after_run_finished() {
    // Regression guard: a prompted run's SSE stream MUST end after RUN_FINISHED.
    // If it stays open (keep-alive / broadcast), the client's runAgent() promise
    // never resolves and assistant-ui's thread is stuck `isRunning` forever.
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("hephaestus", "You are Hephaestus.");
    let config = sandbox.config();

    let run_id_uuid = Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap();
    let session_id = "terminate-session-1";
    let req_body = serde_json::to_vec(&json!({
        "threadId": Uuid::new_v4(),
        "runId": run_id_uuid,
        "state": {},
        "messages": [{ "id": Uuid::new_v4(), "role": "user", "content": "hello" }],
        "tools": [],
        "context": [],
        "forwardedProps": {}
    }))
    .unwrap();

    let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async move {
            harnx_core::sink::emit_agent_event(AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("hi".to_string())],
            }));
            Ok((
                "hi".to_string(),
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

    // Drain the ENTIRE stream to completion (with a timeout). If the stream never
    // ends, this times out and fails — which is exactly the bug we are guarding.
    let mut body = response.into_body().into_data_stream();
    let mut partial = String::new();
    let mut events: Vec<Value> = Vec::new();
    loop {
        let next = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio_stream::StreamExt::next(&mut body),
        )
        .await
        .expect("prompted run SSE stream did not terminate (stayed open past RUN_FINISHED)");
        let Some(chunk) = next else { break }; // stream ended cleanly
        partial.push_str(std::str::from_utf8(&chunk.expect("chunk")).expect("utf8"));
        while let Some(idx) = partial.find("\n\n") {
            let frame = partial[..idx].trim().to_string();
            partial.drain(..idx + 2);
            if frame.is_empty() || frame.starts_with(':') {
                continue;
            }
            events.push(parse_sse_frame(&frame));
        }
    }

    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert!(
        !types.is_empty(),
        "stream should have emitted at least one event"
    );
    assert_eq!(
        types.first(),
        Some(&"RUN_STARTED"),
        "stream must start with RUN_STARTED; got {types:?}"
    );
    assert_eq!(
        types.last(),
        Some(&"RUN_FINISHED"),
        "prompted stream must END on RUN_FINISHED; got {types:?}"
    );
    // No events may follow the terminal RUN_FINISHED.
    let run_finished_idx = types.iter().position(|t| *t == "RUN_FINISHED").unwrap();
    assert_eq!(
        run_finished_idx,
        types.len() - 1,
        "no events may follow RUN_FINISHED; got {types:?}"
    );

    // STEP_STARTED / STEP_FINISHED must be balanced with matching stepName, and all
    // steps closed before RUN_FINISHED (required by @ag-ui/client verifyEvents).
    let mut open_steps: Vec<String> = Vec::new();
    for e in &events {
        match e["type"].as_str().unwrap() {
            "STEP_STARTED" => open_steps.push(e["stepName"].as_str().unwrap_or("").to_string()),
            "STEP_FINISHED" => {
                let name = e["stepName"].as_str().unwrap_or("");
                let popped = open_steps.pop();
                assert_eq!(
                    popped.as_deref(),
                    Some(name),
                    "STEP_FINISHED stepName must match the open STEP_STARTED"
                );
            }
            "RUN_FINISHED" => assert!(
                open_steps.is_empty(),
                "all steps must be closed before RUN_FINISHED; still open: {open_steps:?}"
            ),
            _ => {}
        }
    }
    assert!(open_steps.is_empty(), "unbalanced steps: {open_steps:?}");
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
    // Prompted run: no MESSAGES_SNAPSHOT (pure delta stream).
    assert_eq!(
        event_types,
        vec![
            "RUN_STARTED",
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_END",
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
async fn ag_ui_run_idle_join_snapshot_and_close() {
    // Promptless subscribe now terminates after RUN_FINISHED (no keepalive passthrough).
    // The stream should emit the synthetic envelope and close promptly.
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
        "idle-snapshot-session",
        &serde_json::to_vec(&body).unwrap(),
        None,
    )
    .await
    .expect("idle join response");
    let events = read_sse_events_until(response, |events| {
        events.iter().any(|e| e["type"] == "RUN_FINISHED")
    })
    .await;
    assert_event_type_sequence(
        &events,
        &["RUN_STARTED", "MESSAGES_SNAPSHOT", "RUN_FINISHED"],
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
    // Prompted run: pure delta stream, no MESSAGES_SNAPSHOT. The call_fn above
    // already asserts the last user message ("last user wins") is used as the prompt.
    let event_types = events
        .iter()
        .map(|event| event["type"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        event_types,
        vec![
            "RUN_STARTED",
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_CONTENT",
            "TEXT_MESSAGE_END",
            "RUN_FINISHED",
        ]
    );
    assert_eq!(events[2]["delta"].as_str(), Some("assistant2"));
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

fn decode_sse_bytes_chunks(chunks: Vec<Bytes>) -> Vec<Value> {
    chunks
        .into_iter()
        .flat_map(|chunk| {
            let text = String::from_utf8(chunk.to_vec()).expect("utf8 frame");
            parse_sse_frames(&text)
                .into_iter()
                .map(|frame| parse_sse_frame(&frame))
                .collect::<Vec<_>>()
        })
        .collect()
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
    let _ = config;
    crate::session_actor::load_test_session_messages(agent, session_id)
        .iter()
        .filter(|msg| msg.role.is_user() || msg.role.is_assistant())
        .map(|msg| msg.content.to_text())
        .collect()
}
fn load_session_messages(config: &Config, agent: &str, session_id: &str) -> Vec<HistoryMsg> {
    let _ = config;
    crate::session_actor::load_test_session_messages(agent, session_id)
        .iter()
        .filter(|msg| msg.role.is_user() || msg.role.is_assistant())
        .cloned()
        .collect()
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
    match &snapshot[1] {
        AgUiMessage::Assistant {
            content,
            tool_calls: Some(tool_calls),
            ..
        } => {
            assert_eq!(content.as_deref(), Some("assistant prose"));
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].function.name, "history");
            assert_eq!(tool_calls[0].function.arguments, r#"{"limit":5}"#);
            match &snapshot[2] {
                AgUiMessage::Tool {
                    content,
                    tool_call_id,
                    ..
                } => {
                    assert_eq!(content, "tool output");
                    assert_eq!(tool_call_id, &tool_calls[0].id);
                }
                other => panic!("expected tool snapshot, got: {other:?}"),
            }
        }
        other => panic!("expected assistant snapshot, got: {other:?}"),
    }
}

#[test]
fn history_snapshot_leaves_pending_tool_call_running() {
    let history = vec![HistoryMsg {
        role: MessageRole::Tool,
        content: MessageContent::ToolCalls(MessageContentToolCalls::new(
            vec![harnx_core::tool::ToolResult::new(
                ToolCall::new(
                    "long_running_tool".to_string(),
                    json!({"task": "work"}),
                    Some("call-pending".to_string()),
                    None,
                ),
                json!({
                    "error": "tool response pending (results not yet persisted)"
                }),
            )],
            "still working".to_string(),
            None,
        )),
        id: Some(Uuid::new_v4().to_string()),
        log_seq: Some(7),
        log_timestamp: None,
    }];

    let snapshot = history_messages_for_snapshot(&history);

    assert!(matches!(
        snapshot.as_slice(),
        [AgUiMessage::Assistant {
            content: Some(content),
            tool_calls: Some(tool_calls),
            ..
        }] if content == "still working"
            && tool_calls.len() == 1
            && tool_calls[0].function.name == "long_running_tool"
    ));
}

#[test]
fn history_messages_for_snapshot_keeps_plain_assistant_content() {
    let history = vec![HistoryMsg {
        role: MessageRole::Assistant,
        content: MessageContent::Text("persisted final answer".to_string()),
        id: Some(Uuid::new_v4().to_string()),
        log_seq: Some(4),
        log_timestamp: None,
    }];

    let snapshot = history_messages_for_snapshot(&history);
    assert!(matches!(
        snapshot.as_slice(),
        [AgUiMessage::Assistant { content: Some(content), .. }]
            if content == "persisted final answer"
    ));
}

#[test]
fn history_snapshot_prefers_tool_markdown_and_falls_back_to_output() {
    let tool_call_markdown = ToolCall::new(
        "read_history".to_string(),
        json!({"limit": 1}),
        Some("call-1".to_string()),
        None,
    );
    let mut result_with_markdown =
        harnx_core::tool::ToolResult::new(tool_call_markdown, json!("plain output"));
    result_with_markdown.markdown = Some("### Rendered summary".to_string());

    let tool_call_fallback = ToolCall::new(
        "read_history".to_string(),
        json!({"limit": 2}),
        Some("call-2".to_string()),
        None,
    );
    let result_without_markdown =
        harnx_core::tool::ToolResult::new(tool_call_fallback, json!("fallback output"));

    let history = vec![Message {
        role: MessageRole::Tool,
        content: MessageContent::ToolCalls(MessageContentToolCalls::new(
            vec![result_with_markdown, result_without_markdown],
            "assistant prose".to_string(),
            None,
        )),
        id: Some(Uuid::new_v4().to_string()),
        log_seq: None,
        log_timestamp: None,
    }];

    let snapshot = history_messages_for_snapshot(&history);
    assert_eq!(snapshot.len(), 3);
    match &snapshot[0] {
        AgUiMessage::Assistant {
            content,
            tool_calls: Some(tool_calls),
            ..
        } => {
            assert_eq!(content.as_deref(), Some("assistant prose"));
            assert_eq!(tool_calls.len(), 2);
            assert_eq!(tool_calls[0].function.arguments, r#"{"limit":1}"#);
            assert_eq!(tool_calls[1].function.arguments, r#"{"limit":2}"#);
            assert!(
                matches!(&snapshot[1], AgUiMessage::Tool { content, tool_call_id, .. }
                if content == "### Rendered summary" && tool_call_id == &tool_calls[0].id)
            );
            assert!(
                matches!(&snapshot[2], AgUiMessage::Tool { content, tool_call_id, .. }
                if content == "fallback output" && tool_call_id == &tool_calls[1].id)
            );
        }
        other => panic!("expected assistant snapshot, got: {other:?}"),
    }
}

#[test]
fn history_snapshot_tool_call_id_is_deterministic_without_persisted_ids() {
    // Neither the message id NOR the tool-call id is persisted. The synthesized
    // tool_call_id must be DETERMINISTIC across reloads (derived from the message
    // log sequence) so @assistant-ui keeps re-attaching the tool result to its
    // call. A random fallback would change on every hydration.
    let build_history = || {
        let tool_call = ToolCall::new(
            "history".to_string(),
            json!({ "limit": 5 }),
            None, // no persisted tool-call id
            None,
        );
        vec![Message {
            role: MessageRole::Assistant,
            content: MessageContent::ToolCalls(MessageContentToolCalls::new(
                vec![harnx_core::tool::ToolResult::new(
                    tool_call,
                    json!("tool output"),
                )],
                "assistant prose".to_string(),
                None,
            )),
            id: None, // no persisted message id → random AgUiMessage id, but…
            log_seq: Some(7),
            log_timestamp: None,
        }]
    };

    let first = history_messages_for_snapshot(&build_history());
    let second = history_messages_for_snapshot(&build_history());

    let tool_call_id_of = |snapshot: &[AgUiMessage]| -> ToolCallId {
        match &snapshot[0] {
            AgUiMessage::Assistant {
                tool_calls: Some(tool_calls),
                ..
            } => {
                assert_eq!(tool_calls.len(), 1);
                // Assistant tool_call id and the Tool result tool_call_id must match.
                match &snapshot[1] {
                    AgUiMessage::Tool { tool_call_id, .. } => {
                        assert_eq!(&tool_calls[0].id, tool_call_id);
                    }
                    other => panic!("expected tool snapshot, got: {other:?}"),
                }
                tool_calls[0].id.clone()
            }
            other => panic!("expected assistant snapshot, got: {other:?}"),
        }
    };

    let first_id = tool_call_id_of(&first);
    let second_id = tool_call_id_of(&second);

    // Deterministic across independent hydrations.
    assert_eq!(first_id, second_id);
    // …and derived from the stable log sequence, not a random message id.
    assert_eq!(
        first_id,
        serde_json::from_value(json!("seq:7-tool-0")).unwrap()
    );
}

#[test]
fn history_snapshot_tool_call_id_falls_back_to_ordinal_without_id_or_seq() {
    // Neither message id, tool-call id, NOR log sequence available: fall back to
    // the message ordinal in the history slice — still deterministic per position.
    let tool_call = ToolCall::new("history".to_string(), json!({}), None, None);
    let history = vec![Message {
        role: MessageRole::Assistant,
        content: MessageContent::ToolCalls(MessageContentToolCalls::new(
            vec![harnx_core::tool::ToolResult::new(tool_call, json!("out"))],
            "prose".to_string(),
            None,
        )),
        id: None,
        log_seq: None,
        log_timestamp: None,
    }];

    let snapshot = history_messages_for_snapshot(&history);
    match &snapshot[0] {
        AgUiMessage::Assistant {
            tool_calls: Some(tool_calls),
            ..
        } => {
            assert_eq!(
                tool_calls[0].id,
                serde_json::from_value(json!("ord:0-tool-0")).unwrap()
            );
        }
        other => panic!("expected assistant snapshot, got: {other:?}"),
    }
}

#[tokio::test]
async fn ag_ui_run_promptless_join_forwards_live_events_when_session_active() {
    let snapshot = vec![
        user_msg("persisted prompt"),
        assistant_msg("persisted reply"),
    ];
    let (tx, rx) = tokio::sync::broadcast::channel(8);
    let run_id = Uuid::new_v4();
    let thread_id = Uuid::new_v4();
    let snapshot_frame = Some(Bytes::from(
        frame_event(&snapshot_event(snapshot)).expect("snapshot frame"),
    ));
    let stream = build_promptless_event_stream(
        &run_id.to_string(),
        &thread_id.to_string(),
        snapshot_frame,
        tokio_stream::StreamExt::filter_map(
            tokio_stream::StreamExt::then(
                BroadcastStream::new(rx),
                |item| async move { item.ok() },
            ),
            |event| event,
        ),
        true,
    );

    let sender = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx.send(Event::TextMessageContent(TextMessageContentEvent {
            base: BaseEvent {
                timestamp: None,
                raw_event: None,
            },
            message_id: MessageId::random(),
            delta: "live delta".to_string(),
        }))
        .expect("send live delta");
        tx.send(Event::RunFinished(ag_ui_core::event::RunFinishedEvent {
            base: BaseEvent {
                timestamp: None,
                raw_event: None,
            },
            thread_id: ThreadId::from(thread_id),
            run_id: RunId::from(run_id),
            result: None,
        }))
        .expect("send run finished");
    });

    let frames = tokio_stream::StreamExt::collect::<Vec<_>>(stream).await;
    sender.await.expect("sender task");
    let events = decode_sse_bytes_chunks(frames);

    assert!(
        events.len() >= 4,
        "expected snapshot + live + terminal events: {:?}",
        events
    );
    assert_eq!(events[0]["type"], "RUN_STARTED");
    assert_eq!(events[1]["type"], "MESSAGES_SNAPSHOT");
    let live_index = events
        .iter()
        .position(|event| event["type"] == "TEXT_MESSAGE_CONTENT")
        .expect("live text event should be forwarded");
    let live_start_index = events
        .iter()
        .position(|event| event["type"] == "TEXT_MESSAGE_START")
        .expect("missing synthesized text start");
    let finished_index = events
        .iter()
        .position(|event| event["type"] == "RUN_FINISHED")
        .expect("real run finished should arrive");
    assert!(
        live_start_index < live_index,
        "synthesized start should precede live content: {:?}",
        events
    );
    assert!(
        live_index < finished_index,
        "live event should arrive before terminal: {:?}",
        events
    );
    assert_eq!(
        events[live_start_index]["messageId"],
        events[live_index]["messageId"]
    );
    assert_eq!(events[live_index]["delta"], "live delta");
    assert!(
        events[..finished_index]
            .iter()
            .all(|event| event["type"] != "RUN_FINISHED"),
        "synthetic RUN_FINISHED emitted before live events: {:?}",
        events
    );

    // Regression: the active-reload path must emit exactly ONE RUN_STARTED.
    // Previously it emitted its own boundary and then delegated to the prompted
    // builder, which emitted a second RUN_STARTED for the same run.
    let run_started_count = events
        .iter()
        .filter(|event| event["type"] == "RUN_STARTED")
        .count();
    assert_eq!(
        run_started_count, 1,
        "exactly one RUN_STARTED expected on active-reload; got {run_started_count}: {events:?}"
    );
}

#[tokio::test]
async fn ag_ui_promptless_active_reconnect_synthesizes_text_start_before_unmatched_end() {
    let run_id = Uuid::new_v4();
    let thread_id = Uuid::new_v4();
    let (tx, rx) = tokio::sync::broadcast::channel(16);
    let stream = build_promptless_event_stream(
        &run_id.to_string(),
        &thread_id.to_string(),
        None,
        tokio_stream::StreamExt::filter_map(
            tokio_stream::StreamExt::then(
                BroadcastStream::new(rx),
                |item| async move { item.ok() },
            ),
            |event| event,
        ),
        true,
    );
    let message_id = MessageId::random();

    tx.send(Event::TextMessageEnd(TextMessageEndEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        message_id: message_id.clone(),
    }))
    .expect("send unmatched text end");
    tx.send(Event::RunFinished(ag_ui_core::event::RunFinishedEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        thread_id: ThreadId::from(thread_id),
        run_id: RunId::from(run_id),
        result: None,
    }))
    .expect("send run finished");

    let events = decode_sse_bytes_chunks(tokio_stream::StreamExt::collect::<Vec<_>>(stream).await);
    let start_index = events
        .iter()
        .position(|event| event["type"] == "TEXT_MESSAGE_START")
        .expect("missing synthesized text start");
    let end_index = events
        .iter()
        .position(|event| event["type"] == "TEXT_MESSAGE_END")
        .expect("missing text end");

    assert!(
        start_index < end_index,
        "text start must precede text end: {events:?}"
    );
    assert_eq!(events[start_index]["messageId"], json!(message_id));
    assert_eq!(events[end_index]["messageId"], json!(message_id));
}

#[tokio::test]
async fn ag_ui_promptless_active_reconnect_drops_unmatched_tool_call_events() {
    let run_id = Uuid::new_v4();
    let thread_id = Uuid::new_v4();
    let (tx, rx) = tokio::sync::broadcast::channel(16);
    let stream = build_promptless_event_stream(
        &run_id.to_string(),
        &thread_id.to_string(),
        None,
        tokio_stream::StreamExt::filter_map(
            tokio_stream::StreamExt::then(
                BroadcastStream::new(rx),
                |item| async move { item.ok() },
            ),
            |event| event,
        ),
        true,
    );
    let tool_call_id = ToolCallId::random();
    let message_id = MessageId::random();

    tx.send(Event::ToolCallArgs(ToolCallArgsEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        tool_call_id: tool_call_id.clone(),
        delta: "{\"arg\":1}".to_string(),
    }))
    .expect("send unmatched tool args");
    tx.send(Event::ToolCallEnd(ToolCallEndEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        tool_call_id: tool_call_id.clone(),
    }))
    .expect("send unmatched tool end");
    tx.send(Event::ToolCallResult(ToolCallResultEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        message_id,
        tool_call_id: tool_call_id.clone(),
        content: "done".to_string(),
        role: Role::Tool,
    }))
    .expect("send unmatched tool result");
    tx.send(Event::RunFinished(ag_ui_core::event::RunFinishedEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        thread_id: ThreadId::from(thread_id),
        run_id: RunId::from(run_id),
        result: None,
    }))
    .expect("send run finished");

    let events = decode_sse_bytes_chunks(tokio_stream::StreamExt::collect::<Vec<_>>(stream).await);
    assert!(
        events.iter().all(|event| {
            !matches!(
                event["type"].as_str(),
                Some("TOOL_CALL_ARGS" | "TOOL_CALL_END" | "TOOL_CALL_RESULT")
            )
        }),
        "unmatched tool events should be dropped: {events:?}"
    );
}

#[tokio::test]
async fn ag_ui_promptless_active_reconnect_forwards_started_tool_call_lifecycle() {
    let run_id = Uuid::new_v4();
    let thread_id = Uuid::new_v4();
    let (tx, rx) = tokio::sync::broadcast::channel(16);
    let stream = build_promptless_event_stream(
        &run_id.to_string(),
        &thread_id.to_string(),
        None,
        tokio_stream::StreamExt::filter_map(
            tokio_stream::StreamExt::then(
                BroadcastStream::new(rx),
                |item| async move { item.ok() },
            ),
            |event| event,
        ),
        true,
    );
    let tool_call_id = ToolCallId::random();
    let result_message_id = MessageId::random();

    tx.send(Event::ToolCallStart(ToolCallStartEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        tool_call_id: tool_call_id.clone(),
        tool_call_name: "search".to_string(),
        parent_message_id: None,
    }))
    .expect("send tool start");
    tx.send(Event::ToolCallArgs(ToolCallArgsEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        tool_call_id: tool_call_id.clone(),
        delta: "{\"q\":\"rust\"}".to_string(),
    }))
    .expect("send tool args");
    tx.send(Event::ToolCallEnd(ToolCallEndEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        tool_call_id: tool_call_id.clone(),
    }))
    .expect("send tool end");
    tx.send(Event::ToolCallResult(ToolCallResultEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        message_id: result_message_id,
        tool_call_id: tool_call_id.clone(),
        content: "done".to_string(),
        role: Role::Tool,
    }))
    .expect("send tool result");
    tx.send(Event::RunFinished(ag_ui_core::event::RunFinishedEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        thread_id: ThreadId::from(thread_id),
        run_id: RunId::from(run_id),
        result: None,
    }))
    .expect("send run finished");

    let events = decode_sse_bytes_chunks(tokio_stream::StreamExt::collect::<Vec<_>>(stream).await);
    let tool_events = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .filter(|kind| kind.starts_with("TOOL_CALL_"))
        .collect::<Vec<_>>();
    assert_eq!(
        tool_events,
        vec![
            "TOOL_CALL_START",
            "TOOL_CALL_ARGS",
            "TOOL_CALL_END",
            "TOOL_CALL_RESULT",
        ]
    );
}

#[tokio::test]
async fn ag_ui_promptless_active_reconnect_synthesizes_step_start_before_unmatched_finish() {
    let run_id = Uuid::new_v4();
    let thread_id = Uuid::new_v4();
    let (tx, rx) = tokio::sync::broadcast::channel(16);
    let stream = build_promptless_event_stream(
        &run_id.to_string(),
        &thread_id.to_string(),
        None,
        tokio_stream::StreamExt::filter_map(
            tokio_stream::StreamExt::then(
                BroadcastStream::new(rx),
                |item| async move { item.ok() },
            ),
            |event| event,
        ),
        true,
    );
    let step_name = "turn-99".to_string();

    tx.send(Event::StepFinished(StepFinishedEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        step_name: step_name.clone(),
    }))
    .expect("send unmatched step finish");
    tx.send(Event::RunFinished(ag_ui_core::event::RunFinishedEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        thread_id: ThreadId::from(thread_id),
        run_id: RunId::from(run_id),
        result: None,
    }))
    .expect("send run finished");

    let events = decode_sse_bytes_chunks(tokio_stream::StreamExt::collect::<Vec<_>>(stream).await);
    let start_index = events
        .iter()
        .position(|event| event["type"] == "STEP_STARTED")
        .expect("missing synthesized step start");
    let finish_index = events
        .iter()
        .position(|event| event["type"] == "STEP_FINISHED")
        .expect("missing step finish");

    assert!(
        start_index < finish_index,
        "step start must precede step finish: {events:?}"
    );
    assert_eq!(events[start_index]["stepName"], json!(step_name));
    assert_eq!(events[finish_index]["stepName"], json!(step_name));
}

#[tokio::test]
async fn ag_ui_promptless_active_reconnect_synthesizes_thinking_start_before_unmatched_end() {
    let run_id = Uuid::new_v4();
    let thread_id = Uuid::new_v4();
    let (tx, rx) = tokio::sync::broadcast::channel(16);
    let stream = build_promptless_event_stream(
        &run_id.to_string(),
        &thread_id.to_string(),
        None,
        tokio_stream::StreamExt::filter_map(
            tokio_stream::StreamExt::then(
                BroadcastStream::new(rx),
                |item| async move { item.ok() },
            ),
            |event| event,
        ),
        true,
    );

    tx.send(Event::ThinkingEnd(ThinkingEndEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
    }))
    .expect("send unmatched thinking end");
    tx.send(Event::RunFinished(ag_ui_core::event::RunFinishedEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        thread_id: ThreadId::from(thread_id),
        run_id: RunId::from(run_id),
        result: None,
    }))
    .expect("send run finished");

    let events = decode_sse_bytes_chunks(tokio_stream::StreamExt::collect::<Vec<_>>(stream).await);
    let thinking_start_index = events
        .iter()
        .position(|event| event["type"] == "THINKING_START")
        .expect("missing synthesized thinking start");
    let thinking_end_index = events
        .iter()
        .position(|event| event["type"] == "THINKING_END")
        .expect("missing thinking end");

    assert!(
        thinking_start_index < thinking_end_index,
        "thinking start must precede thinking end: {events:?}"
    );
}

#[tokio::test]
async fn ag_ui_promptless_active_reconnect_synthesizes_thinking_start_and_text_start_before_unmatched_text_end(
) {
    let run_id = Uuid::new_v4();
    let thread_id = Uuid::new_v4();
    let (tx, rx) = tokio::sync::broadcast::channel(16);
    let stream = build_promptless_event_stream(
        &run_id.to_string(),
        &thread_id.to_string(),
        None,
        tokio_stream::StreamExt::filter_map(
            tokio_stream::StreamExt::then(
                BroadcastStream::new(rx),
                |item| async move { item.ok() },
            ),
            |event| event,
        ),
        true,
    );

    tx.send(Event::ThinkingTextMessageEnd(ThinkingTextMessageEndEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
    }))
    .expect("send unmatched thinking text end");
    tx.send(Event::RunFinished(ag_ui_core::event::RunFinishedEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        thread_id: ThreadId::from(thread_id),
        run_id: RunId::from(run_id),
        result: None,
    }))
    .expect("send run finished");

    let events = decode_sse_bytes_chunks(tokio_stream::StreamExt::collect::<Vec<_>>(stream).await);
    let thinking_start_index = events
        .iter()
        .position(|event| event["type"] == "THINKING_START")
        .expect("missing synthesized thinking start");
    let thinking_text_start_index = events
        .iter()
        .position(|event| event["type"] == "THINKING_TEXT_MESSAGE_START")
        .expect("missing synthesized thinking text start");
    let thinking_text_end_index = events
        .iter()
        .position(|event| event["type"] == "THINKING_TEXT_MESSAGE_END")
        .expect("missing thinking text end");

    assert!(
        thinking_start_index < thinking_text_start_index,
        "thinking start must precede thinking text start: {events:?}"
    );
    assert!(
        thinking_text_start_index < thinking_text_end_index,
        "thinking text start must precede thinking text end: {events:?}"
    );
}

#[tokio::test]
async fn ag_ui_promptless_active_reconnect_synthesizes_thinking_start_and_text_start_before_unmatched_text_content(
) {
    let run_id = Uuid::new_v4();
    let thread_id = Uuid::new_v4();
    let (tx, rx) = tokio::sync::broadcast::channel(16);
    let stream = build_promptless_event_stream(
        &run_id.to_string(),
        &thread_id.to_string(),
        None,
        tokio_stream::StreamExt::filter_map(
            tokio_stream::StreamExt::then(
                BroadcastStream::new(rx),
                |item| async move { item.ok() },
            ),
            |event| event,
        ),
        true,
    );

    tx.send(Event::ThinkingTextMessageContent(
        ThinkingTextMessageContentEvent {
            base: BaseEvent {
                timestamp: None,
                raw_event: None,
            },
            delta: "plan".to_string(),
        },
    ))
    .expect("send unmatched thinking text content");
    tx.send(Event::RunFinished(ag_ui_core::event::RunFinishedEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        thread_id: ThreadId::from(thread_id),
        run_id: RunId::from(run_id),
        result: None,
    }))
    .expect("send run finished");

    let events = decode_sse_bytes_chunks(tokio_stream::StreamExt::collect::<Vec<_>>(stream).await);
    let thinking_start_index = events
        .iter()
        .position(|event| event["type"] == "THINKING_START")
        .expect("missing synthesized thinking start");
    let thinking_text_start_index = events
        .iter()
        .position(|event| event["type"] == "THINKING_TEXT_MESSAGE_START")
        .expect("missing synthesized thinking text start");
    let thinking_text_content_index = events
        .iter()
        .position(|event| event["type"] == "THINKING_TEXT_MESSAGE_CONTENT")
        .expect("missing thinking text content");

    assert!(
        thinking_start_index < thinking_text_start_index,
        "thinking start must precede thinking text start: {events:?}"
    );
    assert!(
        thinking_text_start_index < thinking_text_content_index,
        "thinking text start must precede thinking text content: {events:?}"
    );
    assert_eq!(events[thinking_text_content_index]["delta"], "plan");
}

#[test]
fn session_state_is_active_treats_running_and_interrupted_as_live() {
    use crate::session_actor::{PendingInterruptBatch, SessionState};

    let now = chrono::Utc::now();

    // Idle: not active — a promptless reload should terminate with RUN_FINISHED.
    assert!(!session_state_is_active(&SessionState::Idle));

    // Running: active.
    assert!(session_state_is_active(&SessionState::Running {
        run_id: "run-1".into(),
        started_at: now,
    }));

    // Interrupted (awaiting tool approval): active. A reload here must follow the
    // live broadcast so the pending approval prompt reappears, not close the stream.
    let pending = PendingInterruptBatch {
        interrupt_run_id: "run-1".into(),
        text: "approve?".into(),
        attachment_refs: Vec::new(),
        completion_output: String::new(),
        completion_thought: None,
        tool_calls: Vec::new(),
        interrupts: Vec::new(),
        metadata: serde_json::Value::Null,
    };
    assert!(session_state_is_active(&SessionState::Interrupted {
        run_id: "run-1".into(),
        started_at: now,
        pending: Box::new(pending),
    }));
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

#[test]
fn pending_user_prompt_ignores_empty_or_whitespace_user_messages() {
    let empty = parse_run_input(&ag_ui_request_body(Uuid::new_v4(), "")).expect("parse empty");
    assert_eq!(pending_user_prompt(&empty, &[]), None);

    let whitespace = parse_run_input(&ag_ui_request_body(
        Uuid::new_v4(),
        "   
	  ",
    ))
    .expect("parse whitespace");
    assert_eq!(pending_user_prompt(&whitespace, &[]), None);

    let text = parse_run_input(&ag_ui_request_body(Uuid::new_v4(), "hello")).expect("parse text");
    assert_eq!(pending_user_prompt(&text, &[]).as_deref(), Some("hello"));
}

#[test]
fn pending_user_prompt_ignores_a_user_message_already_in_the_snapshot() {
    let message_id = Uuid::new_v4();
    let body = serde_json::to_vec(&json!({
        "threadId": Uuid::new_v4(),
        "runId": Uuid::new_v4(),
        "state": {},
        "messages": [{
            "id": message_id,
            "role": "user",
            "content": "what time is it in London?"
        }],
        "tools": [],
        "context": [],
        "forwardedProps": {}
    }))
    .unwrap();
    let input = parse_run_input(&body).expect("parse hydration request");
    let snapshot = vec![AgUiMessage::User {
        id: MessageId::from(message_id),
        content: "what time is it in London?".to_string(),
        name: None,
    }];

    assert_eq!(pending_user_prompt(&input, &snapshot), None);

    let intentionally_repeated = vec![AgUiMessage::User {
        id: MessageId::random(),
        content: "what time is it in London?".to_string(),
        name: None,
    }];
    assert_eq!(
        pending_user_prompt(&input, &intentionally_repeated).as_deref(),
        Some("what time is it in London?")
    );
}

#[tokio::test]
async fn ag_ui_run_empty_last_user_message_joins_only_and_does_not_start_run() {
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let call_fn: AgentCallFn = {
        let call_count = call_count.clone();
        Arc::new(move |_input, _config, _abort| {
            call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                Ok((
                    "should not run".to_string(),
                    None,
                    Vec::<ToolCall>::new(),
                    CompletionTokenUsage::default(),
                ))
            })
        })
    };
    let registry = ag_ui_test_registry(&config, Some(call_fn));
    let body = ag_ui_request_body(
        Uuid::new_v4(),
        "   
	 ",
    );

    let response =
        ag_ui_run_with_call_fn(&config, &registry, "plain", "empty-last-user", &body, None)
            .await
            .expect("ag-ui response");
    let read = read_sse_until(response, Duration::from_secs(5), |read| {
        read.events
            .iter()
            .any(|event| event["type"] == "RUN_FINISHED")
    })
    .await;

    assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_event_type_sequence(
        &read.events,
        &["RUN_STARTED", "MESSAGES_SNAPSHOT", "RUN_FINISHED"],
    );
    assert!(
        load_session_messages(&config, "plain", "empty-last-user").is_empty(),
        "join-only empty prompt should not persist history"
    );
}
