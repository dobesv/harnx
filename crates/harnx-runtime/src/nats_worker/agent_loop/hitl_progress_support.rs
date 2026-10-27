use crate::ApprovalRef;
use crate::{HitlDecisionReceipt, NatsTurnResult, PendingHitlApproval};

pub(super) struct ExpectedPending<'a> {
    pub request_seq: u64,
    pub tool_round_seq: u64,
    pub tool_call_id: &'a str,
    pub tool_name: &'a str,
    pub arguments: serde_json::Value,
    pub summary: &'a str,
}

pub(super) fn assert_pending_matches(actual: &PendingHitlApproval, expected: ExpectedPending<'_>) {
    assert_eq!(actual.request_seq, expected.request_seq);
    assert_eq!(actual.tool_round_seq, expected.tool_round_seq);
    assert_eq!(actual.tool_call_id, expected.tool_call_id);
    assert_pending_payload(actual, &expected);
}

fn assert_pending_payload(actual: &PendingHitlApproval, expected: &ExpectedPending<'_>) {
    assert_eq!(actual.tool_name, expected.tool_name);
    assert_eq!(actual.arguments, expected.arguments);
    assert_eq!(actual.summary, expected.summary);
}

pub(super) fn assert_approval_ref_matches(actual: &ApprovalRef, expected: &ApprovalRef) {
    assert_eq!(actual.invocation_id, expected.invocation_id);
    assert_eq!(actual.request_seq, expected.request_seq);
    assert_eq!(actual.tool_round_seq, expected.tool_round_seq);
    assert_eq!(actual.tool_call_id, expected.tool_call_id);
}

pub(super) fn assert_pending_roundtrip(pending: &PendingHitlApproval) {
    assert_eq!(
        serde_json::from_value::<PendingHitlApproval>(
            serde_json::to_value(pending.clone()).unwrap()
        )
        .unwrap(),
        *pending
    );
}

pub(super) fn assert_reference_roundtrip(reference: &ApprovalRef) {
    assert_eq!(
        serde_json::from_value::<ApprovalRef>(serde_json::to_value(reference.clone()).unwrap())
            .unwrap(),
        *reference
    );
}

pub(super) fn assert_decision_receipt_matches(
    receipt: &HitlDecisionReceipt,
    expected_request: &ApprovalRef,
    expected_decision_seq: u64,
    expected_approved: bool,
) {
    assert_eq!(receipt.request, *expected_request);
    assert_eq!(receipt.decision_seq, expected_decision_seq);
    assert_eq!(receipt.approved, expected_approved);
    assert_eq!(receipt.note.as_deref(), Some("decision note"));
}

pub(super) struct ExpectedNextPending<'a> {
    pub request_seq: u64,
    pub tool_round_seq: u64,
    pub tool_name: &'a str,
    pub arguments: serde_json::Value,
}

pub(super) fn assert_next_pending(actual: &PendingHitlApproval, expected: ExpectedNextPending<'_>) {
    assert_eq!(actual.request_seq, expected.request_seq);
    assert_eq!(actual.tool_round_seq, expected.tool_round_seq);
    assert_eq!(actual.tool_name, expected.tool_name);
    assert_eq!(actual.arguments, expected.arguments);
}

pub(super) fn assert_terminal_turn_result(
    result: &NatsTurnResult,
    expected_cancelled: bool,
    expected_error: Option<&str>,
) {
    assert_eq!(result.user_msg_id, "prompt");
    assert_eq!(result.user_msg_seq, 1);
    assert_eq!(result.was_cancelled, expected_cancelled);
    assert_eq!(result.error.as_deref(), expected_error);
}
