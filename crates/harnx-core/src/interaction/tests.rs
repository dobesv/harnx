use super::*;
use crate::session::{SessionLogEntry, ToolOutput};

fn identity(payload: &Value) -> InteractionIdentity<'_> {
    InteractionIdentity {
        session: "session-storage-key",
        tool_round_seq: 7,
        tool_call_id: "call-a",
        chain_version: "chain-v1",
        producer: "middleware:confirm-handoff",
        step_path: &[],
        key: "handoff",
        kind: "confirm",
        payload,
    }
}

#[test]
fn interaction_identity_is_stable_and_binds_every_logical_input() {
    let payload = confirm_surface("surface-1", "Hand off?");
    let input = identity(&payload);
    let expected = interaction_id(&input);
    assert_eq!(expected, interaction_id(&input));
    assert_eq!(expected.len(), 64);
    let path = [InteractionStep {
        middleware: "outer".into(),
        key: "run".into(),
    }];
    let changed = [
        InteractionIdentity {
            session: "other",
            ..input.clone()
        },
        InteractionIdentity {
            tool_round_seq: 8,
            ..input.clone()
        },
        InteractionIdentity {
            tool_call_id: "call-b",
            ..input.clone()
        },
        InteractionIdentity {
            chain_version: "chain-v2",
            ..input.clone()
        },
        InteractionIdentity {
            producer: "middleware:other",
            ..input.clone()
        },
        InteractionIdentity {
            step_path: &path,
            ..input.clone()
        },
        InteractionIdentity {
            key: "other",
            ..input.clone()
        },
        InteractionIdentity {
            kind: "other",
            ..input.clone()
        },
    ];
    for input in changed {
        assert_ne!(expected, interaction_id(&input));
    }
    assert_ne!(
        expected,
        interaction_id(&identity(&confirm_surface("surface-1", "Changed text?")))
    );
}

#[test]
fn interaction_identity_ignores_surface_time_and_object_insertion_order() {
    let original = confirm_surface("first", "Proceed?");
    let mut changed = confirm_surface("second", "Proceed?");
    changed["timestamp"] = json!("2026-11-17T00:00:00Z");
    changed["createSurface"]["timestamp"] = json!(123);
    changed["createSurface"]["components"][1]["action"]["surfaceId"] = json!("second");
    changed["createSurface"]["components"][1]["action"]["timestamp"] = json!(124);
    assert_eq!(
        interaction_id(&identity(&original)),
        interaction_id(&identity(&changed))
    );
    let a: Value = serde_json::from_str(r#"{"context":{"b":2,"a":1},"text":"hello"}"#).unwrap();
    let b: Value = serde_json::from_str(r#"{"text":"hello","context":{"a":1,"b":2}}"#).unwrap();
    assert_eq!(interaction_id(&identity(&a)), interaction_id(&identity(&b)));
}

#[test]
fn interaction_identity_keeps_action_semantics_and_user_data() {
    let payload = confirm_surface("surface", "Proceed?");
    let mut changed = payload.clone();
    changed["createSurface"]["components"][1]["action"]["name"] = json!("delete");
    assert_ne!(
        interaction_id(&identity(&payload)),
        interaction_id(&identity(&changed))
    );
    for field in ["timestamp", "surfaceId"] {
        let a = json!({"action": {"context": {field: "first"}}});
        let b = json!({"action": {"context": {field: "second"}}});
        assert_ne!(interaction_id(&identity(&a)), interaction_id(&identity(&b)));
    }
}

pub(super) fn interaction_entries() -> Vec<SessionLogEntry> {
    vec![
        SessionLogEntry::ToolCallCompleted {
            tool_round_seq: 7,
            tool_call_id: "b".into(),
            result: ToolOutput {
                id: Some("b".into()),
                name: "read".into(),
                output: json!("done"),
                markdown: None,
                content: vec![],
                switch_agent: None,
            },
            private_snapshot: "private-result-ref".into(),
            fence_token: 4,
        },
        SessionLogEntry::InteractionRequested {
            tool_round_seq: 7,
            tool_call_id: "a".into(),
            interaction_id: "interaction".into(),
            producer: "middleware:confirm".into(),
            step_path: vec![],
            key: "keep".into(),
            kind: "confirm".into(),
            payload: confirm_surface("surface", "Keep?"),
            summary: "Keep?".into(),
            chain_version: "chain".into(),
            checkpoint: "private-checkpoint-ref".into(),
            fence_token: 4,
        },
        SessionLogEntry::SuspensionReady {
            tool_round_seq: 7,
            pending: vec!["interaction".into()],
            fence_token: 4,
        },
        SessionLogEntry::InteractionResponse {
            request_seq: 9,
            interaction_id: "interaction".into(),
            tool_call_id: "a".into(),
            response: InteractionResponse {
                action: "approve".into(),
                data: Default::default(),
                note: Some("checked".into()),
            },
            fence_token: 5,
        },
    ]
}

#[test]
fn interaction_log_entries_serde_and_fencing_round_trip() {
    for mut entry in interaction_entries() {
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(entry, serde_json::from_str(&json).unwrap());
        let yaml = serde_yaml::to_string(&entry).unwrap();
        assert_eq!(entry, serde_yaml::from_str(&yaml).unwrap());
        assert!(!json.contains("execution_context"));
        entry.set_fence_token(10);
        assert_eq!(entry.fence_token(), Some(10));
    }
}

#[test]
fn interaction_tool_results_handoff_selection_is_optional() {
    let old = r#"{"type":"tool_results","results":[]}"#;
    let entry: SessionLogEntry = serde_json::from_str(old).unwrap();
    assert!(matches!(
        entry,
        SessionLogEntry::ToolResults {
            handoff_selection: None,
            ..
        }
    ));
    assert_eq!(serde_json::to_string(&entry).unwrap(), old);
    for selection in [None, Some("handoff-call".into())] {
        let entry = SessionLogEntry::ToolResults {
            results: vec![],
            handoff_selection: selection,
            timestamp: None,
        };
        assert_eq!(
            entry,
            serde_json::from_value(serde_json::to_value(&entry).unwrap()).unwrap()
        );
    }
}

#[test]
fn interaction_confirm_surface_has_native_confirm_metadata_and_actions() {
    let surface = confirm_surface("surface", "Proceed?");
    assert_eq!(surface["harnx"]["kind"], "confirm");
    let components = &surface["createSurface"]["components"];
    assert_eq!(components[0]["text"], "Proceed?");
    assert_eq!(components[1]["action"]["name"], "approve");
    assert_eq!(components[2]["action"]["name"], "deny");
}

#[test]
fn interaction_records_preserve_pending_round_without_model_results() {
    use crate::message::{MessageContent, MessageRole};
    use crate::session_reconstruct::{reconstruct_state_from_nats, TurnStatus};
    use crate::tool::ToolCall;

    let mut entries = vec![
        (
            1,
            SessionLogEntry::Message {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text("run tools".into()),
                timestamp: None,
                fence_token: None,
            },
        ),
        (
            7,
            SessionLogEntry::ToolCalls {
                calls: vec![ToolCall::new(
                    "read".into(),
                    json!({}),
                    Some("b".into()),
                    None,
                )],
                text: String::new(),
                thought: None,
                timestamp: None,
                fence_token: None,
            },
        ),
    ];
    entries.extend(
        interaction_entries()
            .into_iter()
            .enumerate()
            .map(|(i, entry)| (8 + i as u64, entry)),
    );
    let state = reconstruct_state_from_nats(&entries);
    let TurnStatus::InFlightResumable { orphans } = state.turn_status else {
        panic!("metadata must not close the round")
    };
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].calls[0].id.as_deref(), Some("b"));
    assert!(state.resumable_ctx.unwrap().pending_tool_results.is_empty());
    let view = crate::transcript_view::default_view(&entries).unwrap();
    assert_eq!(
        view.entries, entries,
        "canonical readers must retain control metadata for state hydration"
    );
}

#[test]
fn interaction_response_requires_object_data_and_rejects_forged_fields() {
    for value in [
        json!({"action": "approve", "data": null}),
        json!({"action": "approve", "data": {}, "arguments": {}}),
        json!({"action": "approve"}),
    ] {
        assert!(serde_json::from_value::<InteractionResponse>(value).is_err());
    }
}
