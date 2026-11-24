use super::*;
use harnx_core::session::{SessionLogEntry, ToolOutput};
use serde_json::Value;

fn summaries() -> [Value; 2] {
    [
        json!({"agent":"child","cluster":"shared","session_id":"child-session",
            "kind":"legacy_approval", "request":{"invocation_id":"child-invocation",
                "tool_round_seq":2,"request_seq":3,"tool_call_id":"child-call"},
            "tool_name":"read","message":"Read file?","pending_count":1}),
        json!({"agent":"child","cluster":"shared","session_id":"child-session",
            "kind":"interaction","interaction_id":"confirm-request","request_seq":8,
            "interaction_kind":"confirm","tool_name":"write","message":"Write file?","pending_count":0}),
    ]
}

fn progress(pending: Value) -> SubAgentProgress {
    serde_json::from_value(json!({
        "invocation_id":"child-prompt","agent":"child","session_id":"child-session",
        "status":"running","elapsed_ms":2000,"usage":CompletionTokenUsage::default(),
        "tool_call_count":1,"tool_call_id":"parent-call","pending_interaction":pending,
    }))
    .unwrap()
}

fn next_progress(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Value {
    while let Ok(event) = rx.try_recv() {
        if let Event::Custom(custom) = event {
            if custom.name == "sub_agent_progress" {
                return custom.value;
            }
        }
    }
    panic!("missing sub_agent_progress event")
}

#[test]
fn subagent_pending_interaction_forwarded_live_for_both_kinds_and_clear() {
    for pending in summaries() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = AgUiSink::new(tx, MessageId::from(uuid::Uuid::new_v4()));
        sink.emit(AgentEvent::Turn(TurnEvent::SubAgentProgress(progress(
            pending.clone(),
        ))));
        let event = next_progress(&mut rx);
        assert_eq!(event["pending_interaction"], pending);
        assert_eq!(event["status"], "running");
        sink.emit(AgentEvent::Turn(TurnEvent::SubAgentProgress(progress(
            Value::Null,
        ))));
        assert!(next_progress(&mut rx).get("pending_interaction").is_none());
    }
}

#[test]
fn subagent_pending_interaction_catchup_survives_persisted_tool_results_and_ag_ui_mapping() {
    for pending in summaries() {
        let persisted = progress(pending.clone());
        let entries = vec![(
            4,
            SessionLogEntry::ToolResults {
                results: vec![ToolOutput {
                    user_surfaces: Vec::new(),
                    id: Some("parent-call".into()),
                    name: "session_prompt".into(),
                    output: json!({"status":"blocked","terminal":false,"sub_agent_progress":persisted}),
                    markdown: None,
                    content: vec![],
                    switch_agent: None,
                }],
                handoff_selection: None,
                timestamp: None,
            },
        )];
        let restored = harnx_runtime::nats_session::completed_subagent_progress(&entries, 0);
        assert_eq!(restored.len(), 1);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = AgUiSink::new(tx, MessageId::from(uuid::Uuid::new_v4()));
        for snapshot in restored {
            sink.emit(AgentEvent::Turn(TurnEvent::SubAgentProgress(snapshot)));
        }
        let event = next_progress(&mut rx);
        assert_eq!(event["pending_interaction"], pending);
        assert_eq!(event["tool_call_id"], "parent-call");
    }
}
