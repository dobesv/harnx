use super::*;
use harnx_core::event::{AgentEvent, AgentEventSink, ContentBlock, ModelEvent, NullSink};
use harnx_core::session::{SessionLogEntry, ToolOutput};

#[test]
fn sanitized_empty_durable_progress_does_not_expose_later_live_reasoning() {
    let earlier_thought = "<think>earlier persisted reasoning</think>";
    let entries = thinking_only_tool_round(earlier_thought);
    let durable = crate::PublicProgress::from_entries(&entries, 1);
    assert_eq!(durable, crate::PublicProgress::default());

    let private_reference = "cid:plan:pantheon%2Fatlas/armDRA/private-live-reasoning";
    let later_thought = format!("<think>later live-only reasoning {private_reference}</think>");
    let sink = InvocationBufferingSink::new(Arc::new(NullSink));
    for text in [earlier_thought, later_thought.as_str()] {
        sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text(text.into())],
        }));
    }
    let raw_live = sink.public_progress();
    assert!(raw_live.available);
    assert_eq!(raw_live.references, vec![private_reference]);

    let result = synthesize_termination(
        TerminationSpec {
            timeout: None,
            kind: TerminationKind::Timeout,
            budget: None,
            repetition: None,
        },
        "child",
        &stopped_progress(),
        &sink,
        Some(&durable),
    );
    let termination_json = result.termination_json().to_string();
    for text in [result.response.as_str(), termination_json.as_str()] {
        assert!(!text.contains("later live-only reasoning"));
        assert!(!text.contains(private_reference));
        assert!(!text.contains("earlier persisted reasoning"));
    }
    assert_eq!(result.termination.public_progress, Some(durable));
    assert!(result.termination.thinking_excerpt.is_none());
    assert!(result.response.contains("Public progress unavailable"));
    assert_eq!(
        sink.public_progress(),
        raw_live,
        "shared live buffer stays raw"
    );
    assert!(matches!(
        &entries[0].1,
        SessionLogEntry::ToolCalls { text, .. } if text == earlier_thought
    ));
}

fn thinking_only_tool_round(thought: &str) -> Vec<(u64, SessionLogEntry)> {
    vec![
        (
            2,
            SessionLogEntry::ToolCalls {
                text: thought.into(),
                thought: None,
                calls: vec![harnx_core::tool::ToolCall::new(
                    "noop".into(),
                    serde_json::json!({}),
                    Some("call".into()),
                    None,
                )],
                timestamp: None,
                fence_token: Some(1),
            },
        ),
        (
            3,
            SessionLogEntry::ToolResults {
                results: vec![ToolOutput {
                    id: Some("call".into()),
                    name: "noop".into(),
                    output: serde_json::Value::Null,
                    markdown: None,
                    content: vec![],
                    switch_agent: None,
                }],
                timestamp: None,
            },
        ),
        (
            4,
            SessionLogEntry::cancel_request("timeout".into(), "parent".into()),
        ),
    ]
}

fn stopped_progress() -> SubAgentProgress {
    SubAgentProgress {
        invocation_id: "invocation".into(),
        agent: "helper".into(),
        session_id: "child".into(),
        status: SubAgentProgressStatus::Cancelled,
        elapsed_ms: 10,
        usage: Default::default(),
        tool_call_count: 1,
        title: None,
        tool_call_id: Some("parent-call".into()),
    }
}
