//! Unit tests for interaction derivation and response acceptance.

use super::*;
use anyhow::Result;
use harnx_core::{
    interaction::{InteractionResponse, InteractionStep},
    session::SessionLogEntry,
    tool::ToolCall,
};
use serde_json::json;

use crate::nats_worker::{derive_pending_hitl_approvals, ControlCommand};

#[test]
fn control_command_interaction_response_round_trip() {
    let mut data = serde_json::Map::new();
    data.insert("confirmed".into(), json!(true));
    let resp = InteractionResponse {
        action: "approve".into(),
        data,
        note: Some("approved by user".into()),
    };
    let command = ControlCommand::InteractionResponse {
        interaction_id: "int-123".into(),
        request_seq: 42,
        response: resp.clone(),
    };
    let bytes = command.to_bytes().expect("to bytes");
    let recovered = ControlCommand::from_bytes(&bytes).expect("from bytes");
    assert_eq!(command, recovered);
}

#[test]
fn pending_derivation_gated_by_suspension_ready() {
    let req = (
        2,
        SessionLogEntry::InteractionRequested {
            tool_round_seq: 1,
            tool_call_id: "call-1".into(),
            interaction_id: "int-1".into(),
            producer: "middleware:confirm".into(),
            step_path: vec![InteractionStep {
                middleware: "confirm".into(),
                key: "1".into(),
            }],
            key: "1".into(),
            kind: "confirm".into(),
            payload: json!({"prompt": "Approve this?"}),
            summary: "Approve action".into(),
            chain_version: "v1".into(),
            checkpoint: "cp-1".into(),
            fence_token: 100,
        },
    );

    // 1. Unready: request exists but no SuspensionReady
    let entries = vec![
        (
            1,
            SessionLogEntry::ToolCalls {
                text: String::new(),
                thought: None,
                calls: vec![],
                timestamp: None,
                fence_token: None,
            },
        ),
        req.clone(),
    ];
    let pending = derive_pending_interactions(&entries).expect("derive");
    assert!(
        pending.is_empty(),
        "unready interactions must not be pending"
    );

    // 2. Ready: SuspensionReady includes interaction_id
    let ready_entry = SessionLogEntry::SuspensionReady {
        tool_round_seq: 1,
        pending: vec!["int-1".into()],
        fence_token: 10,
    };
    let ready_entries = vec![entries[0].clone(), entries[1].clone(), (3, ready_entry)];
    let pending = derive_pending_interactions(&ready_entries).expect("derive");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].interaction_id, "int-1");
    assert_eq!(pending[0].request_seq, 2);
    assert_eq!(pending[0].summary(), "Approve action");

    // 3. Accepted: InteractionResponse clears pending
    let data = serde_json::Map::new();
    let resp = InteractionResponse {
        action: "approve".into(),
        data,
        note: None,
    };
    let accepted_entries = vec![
        ready_entries[0].clone(),
        ready_entries[1].clone(),
        ready_entries[2].clone(),
        (
            4,
            SessionLogEntry::InteractionResponse {
                request_seq: 2,
                interaction_id: "int-1".into(),
                tool_call_id: "call-1".into(),
                response: resp,
                fence_token: 10,
            },
        ),
    ];
    let pending = derive_pending_interactions(&accepted_entries).expect("derive");
    assert!(pending.is_empty(), "accepted interaction clears pending");

    // 4. Completed: ToolResults clears pending even without response
    let completed_entries = vec![
        ready_entries[0].clone(),
        ready_entries[1].clone(),
        ready_entries[2].clone(),
        (
            4,
            SessionLogEntry::ToolResults {
                results: vec![],
                handoff_selection: None,
                timestamp: None,
            },
        ),
    ];
    let pending = derive_pending_interactions(&completed_entries).expect("derive");
    assert!(pending.is_empty(), "completed round clears pending");
}

#[test]
fn mixed_round_derives_both_interaction_and_legacy_hitl() -> Result<()> {
    let call_1 = ToolCall::new("deploy".into(), json!({}), Some("call-1".into()), None);
    let call_2 = ToolCall::new("format_disk".into(), json!({}), Some("call-2".into()), None);

    let entries = vec![
        (
            1,
            SessionLogEntry::ToolCalls {
                text: String::new(),
                thought: None,
                calls: vec![call_1, call_2],
                timestamp: None,
                fence_token: Some(10),
            },
        ),
        (
            2,
            SessionLogEntry::InteractionRequested {
                tool_round_seq: 1,
                tool_call_id: "call-1".into(),
                interaction_id: "int-deploy".into(),
                producer: "middleware:deploy".into(),
                step_path: vec![],
                key: "deploy".into(),
                kind: "action".into(),
                payload: json!({"target": "cluster"}),
                summary: "Approve deployment to cluster".into(),
                chain_version: "v1".into(),
                checkpoint: "cp-deploy".into(),
                fence_token: 10,
            },
        ),
        (
            3,
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id: "call-2".into(),
                summary: "Approve formatting disk".into(),
                fence_token: 10,
            },
        ),
        (
            4,
            SessionLogEntry::SuspensionReady {
                tool_round_seq: 1,
                pending: vec!["int-deploy".into()],
                fence_token: 10,
            },
        ),
    ];

    let combined = derive_pending_hitl_approvals(&entries)?;
    assert_eq!(
        combined.len(),
        2,
        "must combine interaction and legacy approval"
    );
    assert!(combined
        .iter()
        .any(|a| a.interaction_id.as_deref() == Some("int-deploy")));
    assert!(combined
        .iter()
        .any(|a| a.interaction_id.is_none() && a.tool_call_id == "call-2"));
    Ok(())
}
