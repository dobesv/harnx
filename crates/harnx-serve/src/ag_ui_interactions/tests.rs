use super::*;
use harnx_core::{
    interaction::InteractionResponse,
    message::{MessageContent, MessageRole},
    tool::ToolCall,
};

pub(crate) fn entries() -> Vec<(u64, SessionLogEntry)> {
    vec![
        (
            1,
            SessionLogEntry::Message {
                id: Some("prompt".into()),
                role: MessageRole::User,
                content: MessageContent::Text("Run these tools".into()),
                timestamp: None,
                fence_token: None,
            },
        ),
        (
            2,
            SessionLogEntry::ToolCalls {
                text: String::new(),
                thought: None,
                timestamp: None,
                fence_token: None,
                calls: vec![
                    ToolCall::new("a".into(), json!({}), Some("a".into()), None),
                    ToolCall::new("b".into(), json!({}), Some("b".into()), None),
                ],
            },
        ),
        (3, request("first", "a", "confirm")),
        (4, request("second", "b", "form")),
    ]
}

fn request(id: &str, tool: &str, kind: &str) -> SessionLogEntry {
    SessionLogEntry::InteractionRequested {
        tool_round_seq: 2,
        tool_call_id: tool.into(),
        interaction_id: id.into(),
        producer: "middleware:confirm".into(),
        step_path: vec![],
        key: "run".into(),
        kind: kind.into(),
        payload: harnx_core::interaction::confirm_surface(id, "Run this tool?"),
        summary: "Run this tool?".into(),
        chain_version: "v1".into(),
        checkpoint: "private-checkpoint".into(),
        fence_token: 1,
    }
}

pub(crate) fn ready() -> SessionLogEntry {
    SessionLogEntry::SuspensionReady {
        tool_round_seq: 2,
        pending: vec!["first".into(), "second".into()],
        fence_token: 1,
    }
}

fn accepted() -> SessionLogEntry {
    SessionLogEntry::InteractionResponse {
        request_seq: 3,
        interaction_id: "first".into(),
        tool_call_id: "a".into(),
        response: InteractionResponse {
            action: "approve".into(),
            data: Default::default(),
            note: None,
        },
        fence_token: 1,
    }
}

fn interaction_events(entries: &[(u64, SessionLogEntry)]) -> Vec<Value> {
    crate::ag_ui::control_snapshot_events(entries, None)
        .into_iter()
        .filter_map(|event| match event {
            Event::Custom(custom) if custom.name == "pending_interaction" => Some(custom.value),
            _ => None,
        })
        .collect()
}

#[test]
fn pending_interaction_is_gated_by_suspension_ready_and_queued_in_manifest_order() {
    let mut entries = entries();
    assert!(interaction_events(&entries).is_empty());
    entries.push((5, ready()));
    assert_eq!(
        interaction_events(&entries),
        vec![json!({
            "interaction_id": "first", "request_seq": 3, "tool_call_id": "a",
            "kind": "confirm", "message": "Run this tool?", "pending_count": 1,
        })]
    );
    entries.push((6, accepted()));
    let events = interaction_events(&entries);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["interaction_id"], "second");
    assert_eq!(events[0]["request_seq"], 4);
    assert_eq!(events[0]["kind"], "form");
    assert_eq!(events[0]["pending_count"], 0);
    assert!(!events[0].to_string().contains("private-checkpoint"));
    entries.push((
        7,
        SessionLogEntry::Cancel {
            requested_by: None,
            fence_token: 0,
            cancellation_id: None,
            timestamp: None,
        },
    ));
    assert_eq!(interaction_events(&entries), vec![Value::Null]);
}

#[test]
fn pending_interaction_uses_latest_manifest() {
    let mut entries = entries();
    entries.push((5, ready()));
    entries.push((
        6,
        SessionLogEntry::SuspensionReady {
            tool_round_seq: 2,
            pending: vec!["second".into()],
            fence_token: 1,
        },
    ));
    assert_eq!(pending_value(&entries).unwrap()["interaction_id"], "second");
}

#[test]
fn pending_interaction_and_legacy_approval_keep_separate_event_shapes() {
    let mut entries = entries();
    entries.push((
        5,
        SessionLogEntry::HitlApprovalRequested {
            tool_call_id: "legacy".into(),
            summary: "Legacy approval".into(),
            fence_token: 1,
        },
    ));
    entries.push((6, ready()));
    let events = crate::ag_ui::control_snapshot_events(&entries, None);
    let custom: Vec<_> = events
        .into_iter()
        .filter_map(|event| match event {
            Event::Custom(event) => Some((event.name, event.value)),
            _ => None,
        })
        .collect();
    assert_eq!(
        custom[0],
        (
            "hitl_pending_approval".into(),
            json!({"tool_call_id":"legacy", "summary":"Legacy approval"})
        )
    );
    assert_eq!(custom[1].0, "pending_interaction");
}

#[test]
fn remote_follow_finishes_attachment_at_ready_interaction_not_at_request() {
    use crate::ag_ui_remote_follow::{terminal_from_history, RemoteFollowTerminal};
    let mut entries = entries();
    assert_eq!(terminal_from_history(&entries, 1), None);
    entries.push((5, ready()));
    assert_eq!(
        terminal_from_history(&entries, 1),
        Some(RemoteFollowTerminal::Finished)
    );
    // This is a modal attachment boundary, not completion of the durable prompt.
    assert_eq!(
        harnx_core::session_reconstruct::pending_prompt_seq(&entries),
        Some(1)
    );
}

#[test]
fn pending_interaction_includes_payload_for_a2ui_kind() {
    let a2ui_payload = json!({
        "version": "1.0",
        "createSurface": {
            "surfaceId": "surf-1",
            "components": [
                { "id": "root", "component": "Card", "child": "btn" },
                { "id": "label", "component": "Text", "text": "Click" },
                { "id": "btn", "component": "Button", "child": "label", "action": { "event": { "name": "click" } } }
            ]
        }
    });

    let entries = vec![
        (
            1,
            SessionLogEntry::ToolCalls {
                text: String::new(),
                thought: None,
                timestamp: None,
                fence_token: None,
                calls: vec![ToolCall::new(
                    "tool_1".into(),
                    json!({}),
                    Some("tool_1".into()),
                    None,
                )],
            },
        ),
        (
            2,
            SessionLogEntry::InteractionRequested {
                tool_round_seq: 1,
                tool_call_id: "tool_1".into(),
                interaction_id: "a2ui_int".into(),
                producer: "middleware:interact".into(),
                step_path: vec![],
                key: "1".into(),
                kind: "a2ui".into(),
                payload: a2ui_payload.clone(),
                summary: "Form summary".into(),
                chain_version: "v1".into(),
                checkpoint: "private".into(),
                fence_token: 1,
            },
        ),
        (
            3,
            SessionLogEntry::SuspensionReady {
                tool_round_seq: 1,
                pending: vec!["a2ui_int".into()],
                fence_token: 1,
            },
        ),
    ];

    let val = pending_value(&entries).expect("pending value");
    assert_eq!(val["kind"], "a2ui");
    assert_eq!(val["payload"], a2ui_payload);
}

#[test]
fn pending_interaction_includes_payload_for_url_kind() {
    let url_payload = json!({
        "url": "https://example.com/connect/abc",
        "message": "Connect Jira"
    });
    let url_request = SessionLogEntry::InteractionRequested {
        tool_round_seq: 1,
        tool_call_id: "tool_1".into(),
        interaction_id: "url_int".into(),
        producer: "tool:jira/connect".into(),
        step_path: vec![],
        key: "connect".into(),
        kind: "url".into(),
        payload: url_payload.clone(),
        summary: "Connect Jira".into(),
        chain_version: "v1".into(),
        checkpoint: "private".into(),
        fence_token: 1,
    };
    let entries = vec![
        (
            1,
            SessionLogEntry::ToolCalls {
                text: String::new(),
                thought: None,
                timestamp: None,
                fence_token: None,
                calls: vec![ToolCall::new(
                    "tool_1".into(),
                    json!({}),
                    Some("tool_1".into()),
                    None,
                )],
            },
        ),
        (2, url_request),
        (
            3,
            SessionLogEntry::SuspensionReady {
                tool_round_seq: 1,
                pending: vec!["url_int".into()],
                fence_token: 1,
            },
        ),
    ];

    let val = pending_value(&entries).expect("pending value");
    assert_eq!(val["kind"], "url");
    assert_eq!(val["payload"], url_payload);
}
