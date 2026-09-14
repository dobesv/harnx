use super::*;
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::event::{
    AgentEvent, AgentEventSink, SubAgentProgress, SubAgentProgressStatus, TurnEvent,
};
use serde_json::json;

#[test]
fn maps_subagent_start_and_progress_custom_events() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let sink = AgUiSink::new(tx, MessageId::from(uuid::Uuid::new_v4()));
    sink.emit(AgentEvent::Turn(TurnEvent::SubAgentStarted {
        agent: "researcher".into(),
        session_id: "child-session".into(),
        invocation_id: Some("inv-1".into()),
    }));
    sink.emit(AgentEvent::Turn(TurnEvent::SubAgentProgress(
        SubAgentProgress {
            invocation_id: "inv-1".into(),
            agent: "researcher".into(),
            session_id: "child-session".into(),
            status: SubAgentProgressStatus::Running,
            elapsed_ms: 10_000,
            usage: CompletionTokenUsage::new(Some(120), Some(45), Some(30)),
            tool_call_count: 3,
            title: None,
        },
    )));

    let Event::Custom(start) = rx.try_recv().expect("sub-agent start") else {
        panic!("expected sub-agent start custom event");
    };
    assert_eq!(start.name, "sub_agent_started");
    assert_eq!(start.value["invocation_id"], json!("inv-1"));

    let Event::Custom(progress) = rx.try_recv().expect("sub-agent progress") else {
        panic!("expected sub-agent progress custom event");
    };
    assert_eq!(progress.name, "sub_agent_progress");
    assert_eq!(progress.value["status"], json!("running"));
    assert_eq!(progress.value["elapsed_ms"], json!(10_000));
    assert_eq!(progress.value["usage"]["cached_tokens"], json!(30));
    assert_eq!(progress.value["tool_call_count"], json!(3));
    // title is absent when None
    assert!(
        progress.value.get("title").is_none(),
        "title should be absent when None"
    );
}

#[test]
fn sub_agent_progress_title_serializes_when_present() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let sink = AgUiSink::new(tx, MessageId::from(uuid::Uuid::new_v4()));
    sink.emit(AgentEvent::Turn(TurnEvent::SubAgentProgress(
        SubAgentProgress {
            invocation_id: "inv-2".into(),
            agent: "researcher".into(),
            session_id: "child-session".into(),
            status: SubAgentProgressStatus::Running,
            elapsed_ms: 5_000,
            usage: CompletionTokenUsage::new(Some(50), Some(25), Some(10)),
            tool_call_count: 1,
            title: Some("Research child".into()),
        },
    )));

    let Event::Custom(progress) = rx.try_recv().expect("sub-agent progress") else {
        panic!("expected sub-agent progress custom event");
    };
    assert_eq!(progress.name, "sub_agent_progress");
    assert_eq!(progress.value["title"], json!("Research child"));
}

#[test]
fn sub_agent_progress_handles_legacy_payload_without_title() {
    // Simulate a legacy payload without the title field — should deserialize without error
    let legacy_json = json!({
        "invocation_id": "inv-legacy",
        "agent": "researcher",
        "session_id": "child-session",
        "status": "running",
        "elapsed_ms": 1000,
        "usage": { "input_tokens": 10, "output_tokens": 5, "cached_tokens": 2 },
        "tool_call_count": 1
    });
    let progress: SubAgentProgress =
        serde_json::from_value(legacy_json).expect("legacy payload should deserialize");
    assert_eq!(progress.invocation_id, "inv-legacy");
    assert!(
        progress.title.is_none(),
        "title should be None for legacy payload"
    );
}
