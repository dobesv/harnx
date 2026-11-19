use super::{derive_hitl_tool_round_continuation, tool_calls_entry};
use harnx_core::session::SessionLogEntry;

#[test]
fn interaction_continuation_preserves_legacy_approval_for_same_call() -> anyhow::Result<()> {
    let entries = vec![
        (1, tool_calls_entry("mixed-call")),
        (
            2,
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id: "mixed-call".into(),
                summary: "Legacy approval".into(),
                fence_token: 7,
            },
        ),
        (
            3,
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id: "mixed-call".into(),
                approved: true,
                note: Some("already approved".into()),
                fence_token: 7,
            },
        ),
        (
            4,
            SessionLogEntry::InteractionRequested {
                tool_round_seq: 1,
                tool_call_id: "mixed-call".into(),
                interaction_id: "middleware-confirm".into(),
                producer: "gate".into(),
                step_path: vec![],
                key: "1".into(),
                kind: "confirm".into(),
                payload: serde_json::json!({"message":"Continue?"}),
                summary: "Continue?".into(),
                chain_version: "chain".into(),
                checkpoint: "private-checkpoint".into(),
                fence_token: 7,
            },
        ),
        (
            5,
            SessionLogEntry::SuspensionReady {
                tool_round_seq: 1,
                pending: vec!["middleware-confirm".into()],
                fence_token: 7,
            },
        ),
    ];
    let continuation = derive_hitl_tool_round_continuation(&entries)?.expect("live continuation");
    assert_eq!(continuation.round.seq, 1);
    assert_eq!(continuation.decisions.len(), 1);
    let decision = &continuation.decisions[0];
    assert_eq!(decision.tool_call_id, "mixed-call");
    assert!(decision.approved);
    assert_eq!(decision.reason.as_deref(), Some("already approved"));
    Ok(())
}
