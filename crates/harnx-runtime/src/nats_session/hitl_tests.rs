use super::*;
use harnx_core::{
    message::{MessageContent, MessageRole},
    tool::ToolCall,
};
use serde_json::json;

const CALL: &str = "reused-call";

fn calls(id: &str) -> SessionLogEntry {
    SessionLogEntry::ToolCalls {
        text: "await approval".into(),
        thought: None,
        calls: vec![ToolCall::new(
            "read".into(),
            json!({}),
            Some(id.into()),
            None,
        )],
        timestamp: None,
        fence_token: Some(1),
    }
}

fn request() -> SessionLogEntry {
    SessionLogEntry::HitlApprovalRequested {
        tool_call_id: CALL.into(),
        summary: "approve read".into(),
        fence_token: 1,
    }
}

fn decision(approved: bool) -> SessionLogEntry {
    SessionLogEntry::HitlApprovalDecision {
        tool_call_id: CALL.into(),
        approved,
        note: None,
        fence_token: 1,
    }
}

fn results() -> SessionLogEntry {
    SessionLogEntry::ToolResults {
        results: vec![],
        timestamp: None,
    }
}

fn pending() -> Vec<(u64, SessionLogEntry)> {
    vec![(1, calls(CALL)), (2, request())]
}

fn original() -> ApprovalRequest<'static> {
    ApprovalRequest {
        tool_call_id: CALL,
        tool_round_seq: 1,
        request_seq: 2,
    }
}

#[test]
fn nats_tool_confirmation_matches_approval_and_denial_after_completion() -> Result<()> {
    for approved in [false, true] {
        let mut entries = pending();
        assert_eq!(original().outcome(&entries, approved)?, None);
        entries.extend([(3, decision(approved)), (4, results())]);
        assert!(already_decided(&entries, CALL, approved)?);
        assert!(!already_decided(&entries, CALL, !approved)?);
        assert_eq!(original().outcome(&entries, approved)?, Some(true));
        assert_eq!(original().outcome(&entries, !approved)?, Some(false));
    }
    Ok(())
}

#[test]
fn nats_tool_confirmation_reused_id_requires_its_own_request_decision() -> Result<()> {
    let mut entries = pending();
    entries.extend([(3, decision(true)), (4, results()), (5, calls(CALL))]);
    assert!(
        !already_decided(&entries, CALL, true)?,
        "old round is not this round"
    );
    entries.push((6, request()));
    assert!(!already_decided(&entries, CALL, true)?);
    // A retry targeting the old round recovers its commit, not the new pending request.
    assert_eq!(original().outcome(&entries, true)?, Some(true));
    entries.extend([(7, decision(false)), (8, results())]);
    assert!(!already_decided(&entries, CALL, true)?);
    assert!(already_decided(&entries, CALL, false)?);
    Ok(())
}

#[test]
fn nats_tool_confirmation_replacement_request_cannot_satisfy_original() -> Result<()> {
    let mut entries = pending();
    entries.extend([(3, request()), (4, decision(true))]);
    assert_eq!(original().outcome(&entries, true)?, Some(false));
    assert!(already_decided(&entries, CALL, true)?);
    Ok(())
}

#[test]
fn nats_tool_confirmation_late_decision_outside_round_is_not_evidence() -> Result<()> {
    for boundary in [
        results(),
        calls("different-call"),
        SessionLogEntry::Cancel {
            fence_token: 1,
            cancellation_id: None,
            requested_by: None,
            timestamp: None,
        },
    ] {
        let mut entries = pending();
        entries.extend([(3, boundary), (4, decision(true))]);
        assert!(!already_decided(&entries, CALL, true)?);
        assert_eq!(original().outcome(&entries, true)?, Some(false));
    }
    Ok(())
}

#[test]
fn nats_tool_confirmation_cancel_without_decision_is_not_success() -> Result<()> {
    let mut entries = pending();
    entries.push((
        3,
        SessionLogEntry::Cancel {
            fence_token: 1,
            cancellation_id: None,
            requested_by: None,
            timestamp: None,
        },
    ));
    assert_eq!(original().outcome(&entries, true)?, Some(false));
    assert!(!already_decided(&entries, CALL, true)?);
    Ok(())
}

#[test]
fn nats_tool_confirmation_decision_requires_a_preceding_request_and_matching_id() -> Result<()> {
    let entries = vec![(1, calls(CALL)), (2, decision(true)), (3, request())];
    assert!(!already_decided(&entries, CALL, true)?);
    let entries = vec![
        (1, calls("different-call")),
        (2, request()),
        (3, decision(true)),
    ];
    assert!(!already_decided(&entries, CALL, true)?);
    Ok(())
}

#[test]
fn nats_tool_confirmation_removed_request_or_decision_is_not_recovered() -> Result<()> {
    for mutation in [
        SessionLogEntry::Rewind { after_seq: 2 },
        SessionLogEntry::EditEntries {
            from: 3,
            to: 3,
            replacements: vec![],
        },
        SessionLogEntry::EditEntries {
            from: 2,
            to: 2,
            replacements: vec![],
        },
    ] {
        let mut entries = pending();
        entries.extend([(3, decision(true)), (4, results()), (5, mutation)]);
        assert!(!already_decided(&entries, CALL, true)?);
    }
    Ok(())
}

#[test]
fn nats_tool_confirmation_retry_does_not_adopt_replacement_round_after_rewind() -> Result<()> {
    let user = SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("prompt".into()),
        timestamp: None,
        fence_token: None,
    };
    let entries = vec![
        (1, user),
        (2, calls(CALL)),
        (3, request()),
        (4, SessionLogEntry::Rewind { after_seq: 1 }),
        (5, calls(CALL)),
        (6, request()),
        (7, decision(true)),
    ];
    let original = ApprovalRequest {
        tool_call_id: CALL,
        tool_round_seq: 2,
        request_seq: 3,
    };
    assert_eq!(original.outcome(&entries, true)?, Some(false));
    assert!(already_decided(&entries, CALL, true)?);
    Ok(())
}
