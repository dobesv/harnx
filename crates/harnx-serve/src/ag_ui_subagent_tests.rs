use super::*;
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::event::{
    AgentEvent, AgentEventSink, SubAgentProgress, SubAgentProgressStatus, TurnEvent,
};
use serde_json::json;

#[test]
fn maps_subagent_progress_custom_event() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let sink = AgUiSink::new(tx, MessageId::from(uuid::Uuid::new_v4()));
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
            tool_call_id: Some("call-1".into()),
        },
    )));

    let Event::Custom(progress) = rx.try_recv().expect("sub-agent progress") else {
        panic!("expected sub-agent progress custom event");
    };
    assert_eq!(progress.name, "sub_agent_progress");
    assert_eq!(progress.value["invocation_id"], json!("inv-1"));
    assert_eq!(progress.value["tool_call_id"], json!("call-1"));
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
            tool_call_id: None,
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

#[test]
fn subagent_progress_emits_both_legacy_and_tool_update() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let sink = AgUiSink::new(tx, MessageId::from(uuid::Uuid::new_v4()));
    sink.emit(AgentEvent::Turn(TurnEvent::SubAgentProgress(
        SubAgentProgress {
            invocation_id: "inv-proj".into(),
            agent: "atlas".into(),
            session_id: "child-proj".into(),
            status: SubAgentProgressStatus::Running,
            elapsed_ms: 5000,
            usage: CompletionTokenUsage::new(Some(100), Some(50), Some(0)),
            tool_call_count: 3,
            title: Some("Analyzing code".into()),
            tool_call_id: None,
        },
    )));

    // First event: legacy sub_agent_progress custom event
    let Event::Custom(progress) = rx.try_recv().expect("sub-agent progress custom event") else {
        panic!("expected sub-agent progress custom event");
    };
    assert_eq!(progress.name, "sub_agent_progress");
    assert_eq!(progress.value["status"], json!("running"));

    // Second event: projected tool_update
    let Event::Custom(update) = rx.try_recv().expect("tool_update event") else {
        panic!("expected tool_update custom event");
    };
    assert_eq!(update.name, "tool_update");
    assert_eq!(update.value["tool_call_id"], json!("inv-proj"));
    let title = update.value["title"]
        .as_str()
        .expect("title should be string");
    assert!(
        title.contains("atlas"),
        "projected title should contain agent: {title}"
    );
    assert!(
        title.contains("Analyzing code"),
        "projected title should contain child title: {title}"
    );
    assert!(
        title.contains("(100→50)"),
        "projected title should contain compact usage: {title}"
    );
}

#[test]
fn subagent_progress_projected_with_zero_usage() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let sink = AgUiSink::new(tx, MessageId::from(uuid::Uuid::new_v4()));
    sink.emit(AgentEvent::Turn(TurnEvent::SubAgentProgress(
        SubAgentProgress {
            invocation_id: "inv-zero".into(),
            agent: "pytheas".into(),
            session_id: "child-zero".into(),
            status: SubAgentProgressStatus::Running,
            elapsed_ms: 1000,
            usage: CompletionTokenUsage::new(None, None, None), // zero usage
            tool_call_count: 0,
            title: None,
            tool_call_id: None,
        },
    )));

    // Drain the legacy event first
    let _ = rx.try_recv().expect("legacy event");

    // Second event: projected tool_update
    let Event::Custom(update) = rx.try_recv().expect("tool_update event") else {
        panic!("expected tool_update custom event");
    };
    let title = update.value["title"]
        .as_str()
        .expect("title should be string");
    // When usage is zero, the usage part should NOT appear in title
    assert!(
        !title.contains("→"),
        "title should not contain usage arrow when usage is zero: {title}"
    );
    assert!(
        title.contains("pytheas"),
        "title should contain agent name: {title}"
    );
}

#[test]
fn subagent_progress_projected_status_mapping() {
    for (progress_status, expected_status) in [
        (SubAgentProgressStatus::Running, "InProgress"),
        (SubAgentProgressStatus::Cancelling, "InProgress"),
        (SubAgentProgressStatus::Unconfirmed, "InProgress"),
        (SubAgentProgressStatus::Done, "InProgress"),
        (SubAgentProgressStatus::Cancelled, "Failed"),
        (SubAgentProgressStatus::Failed, "Failed"),
    ] {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let sink = AgUiSink::new(tx, MessageId::from(uuid::Uuid::new_v4()));
        sink.emit(AgentEvent::Turn(TurnEvent::SubAgentProgress(
            SubAgentProgress {
                invocation_id: "inv-status".into(),
                agent: "researcher".into(),
                session_id: "child-status".into(),
                status: progress_status,
                elapsed_ms: 1000,
                usage: CompletionTokenUsage::default(),
                tool_call_count: 0,
                title: None,
                tool_call_id: None,
            },
        )));

        // Drain legacy event
        let _ = rx.try_recv().expect("legacy event");

        // Projected tool_update
        let Event::Custom(update) = rx.try_recv().expect("tool_update event") else {
            panic!("expected tool_update custom event");
        };
        assert_eq!(
            update.value["status"],
            json!(expected_status),
            "{progress_status:?} should map to {expected_status}"
        );
    }
}
