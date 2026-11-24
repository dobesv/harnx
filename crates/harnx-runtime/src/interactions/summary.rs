//! Parent-facing view of the same pending queue used by the child.
use anyhow::Result;
use harnx_core::{
    pending_interaction::{
        PendingInteractionRequest, PendingInteractionSummary, PENDING_INTERACTION_MESSAGE_MAX_BYTES,
    },
    session::SessionLogEntry,
};

/// Summarize only this session's first pending request. Nested progress embedded
/// in tool results isn't input to pending-state derivation.
pub fn derive_pending_interaction_summary(
    entries: &[(u64, SessionLogEntry)],
    agent: &str,
    cluster: &str,
    session_id: &str,
    invocation_id: &str,
) -> Result<Option<PendingInteractionSummary>> {
    let pending = crate::nats_worker::derive_pending_hitl_approvals(entries)?;
    let Some(first) = pending.first() else {
        return Ok(None);
    };
    let request = if let Some(id) = &first.interaction_id {
        let interactions = super::derive_pending_interactions(entries)?;
        let Some(interaction) = interactions
            .iter()
            .find(|i| i.interaction_id == *id && i.request_seq == first.request_seq)
        else {
            return Ok(None);
        };
        PendingInteractionRequest::Interaction {
            interaction_id: id.clone(),
            request_seq: first.request_seq,
            interaction_kind: interaction.request.kind.clone(),
        }
    } else {
        PendingInteractionRequest::LegacyApproval {
            request: first.approval_ref(invocation_id),
        }
    };
    let mut message = first.summary.clone();
    let mut end = message.len().min(PENDING_INTERACTION_MESSAGE_MAX_BYTES);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    Ok(Some(PendingInteractionSummary {
        agent: agent.to_owned(),
        cluster: cluster.to_owned(),
        session_id: session_id.to_owned(),
        request,
        tool_name: first.tool_name.clone(),
        message,
        pending_count: pending.len() - 1,
    }))
}
