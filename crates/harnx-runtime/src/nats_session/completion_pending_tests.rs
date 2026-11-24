use super::*;
use harnx_core::event::{AgentEvent, AgentEventSink, SubAgentProgressStatus, TurnEvent};
use parking_lot::Mutex;
use serde_json::json;

#[derive(Default)]
struct Sink(Mutex<Vec<AgentEvent>>);
impl AgentEventSink for Sink {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().push(event);
    }
}

fn result(seq: u64, request_seq: Option<u64>) -> (u64, SessionLogEntry) {
    let pending = request_seq.map(|request_seq| {
        json!({
            "agent": "child", "cluster": "local", "session_id": "session",
            "kind": "interaction", "interaction_id": "request", "request_seq": request_seq,
            "interaction_kind": "confirm", "tool_name": "probe_run", "message": "Proceed?",
            "pending_count": 0,
        })
    });
    let output = json!({"sub_agent_progress": {
        "invocation_id": "same-invocation", "agent": "child", "session_id": "session",
        "status": if pending.is_some() { "running" } else { "done" },
        "elapsed_ms": 1, "usage": harnx_core::api_types::CompletionTokenUsage::default(), "tool_call_count": 1, "pending_interaction": pending,
    }});
    let entry = SessionLogEntry::ToolResults {
        results: vec![harnx_core::session::ToolOutput {
            user_surfaces: Vec::new(),
            id: Some("call".into()),
            name: "session_wait".into(),
            output,
            markdown: None,
            content: vec![],
            switch_agent: None,
        }],
        handoff_selection: None,
        timestamp: None,
    };
    (seq, entry)
}

#[test]
fn subagent_blocked_catchup_deduplicates_requests_without_suppressing_final_progress() {
    let sink = Arc::new(Sink::default());
    let event_sink: Arc<dyn AgentEventSink> = sink.clone();
    let mut emitted = HashSet::new();
    let mut entries = vec![result(1, Some(12))];
    reconcile_subagent_progress(&entries, 0, &event_sink, &mut emitted);
    reconcile_subagent_progress(&entries, 0, &event_sink, &mut emitted);
    entries.push(result(2, Some(19)));
    reconcile_subagent_progress(&entries, 0, &event_sink, &mut emitted);
    entries.push(result(3, None));
    reconcile_subagent_progress(&entries, 0, &event_sink, &mut emitted);
    reconcile_subagent_progress(&entries, 0, &event_sink, &mut emitted);
    let events = sink.0.lock();
    let snapshots: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Turn(TurnEvent::SubAgentProgress(progress)) => Some(progress),
            _ => None,
        })
        .collect();
    assert_eq!(snapshots.len(), 3);
    assert!(snapshots[0].pending_interaction.is_some());
    assert!(snapshots[1].pending_interaction.is_some());
    assert_eq!(snapshots[2].status, SubAgentProgressStatus::Done);
    assert!(snapshots[2].pending_interaction.is_none());
}
