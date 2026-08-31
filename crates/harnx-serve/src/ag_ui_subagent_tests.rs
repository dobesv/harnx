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
}
