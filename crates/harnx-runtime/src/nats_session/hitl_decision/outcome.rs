//! Classify only the selected approval generation.
use super::*;

pub(super) fn is_matching_pending(
    pending: &crate::nats_session::PendingHitlApproval,
    target: &HitlDecisionTarget<'_>,
) -> bool {
    pending.approval_ref(&target.reference.invocation_id) == *target.reference
}

pub(super) fn receipt_outcome(
    receipt: crate::nats_session::HitlDecisionReceipt,
    target_approved: bool,
) -> HitlDecisionOutcome {
    if receipt.approved == target_approved {
        HitlDecisionOutcome::AlreadyApplied {
            approved: receipt.approved,
            note: receipt.note,
        }
    } else {
        HitlDecisionOutcome::Conflicting
    }
}
