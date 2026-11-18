use super::derive_pending_interaction_summary;
use harnx_core::{
    interaction::InteractionResponse, pending_interaction::PendingInteractionRequest,
    session::SessionLogEntry, tool::ToolCall,
};
use serde_json::json;

fn request(id: &str, call: &str) -> SessionLogEntry {
    SessionLogEntry::InteractionRequested {
        tool_round_seq: 1,
        tool_call_id: call.into(),
        interaction_id: id.into(),
        producer: "middleware:gate".into(),
        step_path: vec![],
        key: "gate".into(),
        kind: "confirm".into(),
        payload: json!({}),
        summary: "🦀".repeat(129),
        chain_version: "v1".into(),
        checkpoint: "private".into(),
        fence_token: 1,
    }
}

#[test]
fn subagent_blocked_summary_follows_manifest_order_exact_sequence_and_byte_cap() {
    let mut entries = vec![
        (
            1,
            SessionLogEntry::ToolCalls {
                text: String::new(),
                thought: None,
                calls: vec![
                    ToolCall::new("first".into(), json!({}), Some("a".into()), None),
                    ToolCall::new("second".into(), json!({}), Some("b".into()), None),
                ],
                timestamp: None,
                fence_token: None,
            },
        ),
        (2, request("int-a", "a")),
        (3, request("int-b", "b")),
    ];
    let derive = |entries: &[(u64, SessionLogEntry)]| {
        derive_pending_interaction_summary(entries, "helper", "prod", "child", "invocation")
            .unwrap()
    };
    assert!(derive(&entries).is_none());
    entries.push((
        4,
        SessionLogEntry::SuspensionReady {
            tool_round_seq: 1,
            pending: vec!["int-b".into(), "int-a".into()],
            fence_token: 1,
        },
    ));
    let summary = derive(&entries).unwrap();
    assert_eq!(summary.cluster, "prod");
    assert_eq!(summary.tool_name, "second");
    assert_eq!(summary.pending_count, 1);
    assert_eq!(summary.message, "🦀".repeat(128));
    assert!(
        matches!(summary.request, PendingInteractionRequest::Interaction { interaction_id, request_seq: 3, .. } if interaction_id == "int-b")
    );
    entries.push((
        5,
        SessionLogEntry::InteractionResponse {
            interaction_id: "int-b".into(),
            request_seq: 3,
            tool_call_id: "b".into(),
            response: InteractionResponse {
                action: "approve".into(),
                data: Default::default(),
                note: None,
            },
            fence_token: 1,
        },
    ));
    let next = derive(&entries).unwrap();
    assert_eq!(next.tool_name, "first");
    assert_eq!(next.pending_count, 0);
    entries.push((
        6,
        SessionLogEntry::ToolResults {
            results: vec![],
            handoff_selection: None,
            timestamp: None,
        },
    ));
    assert!(derive(&entries).is_none());
}
