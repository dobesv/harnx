//! Recover decision acknowledgements from the original request, not a reused tool ID.
use anyhow::Result;
use harnx_core::{session::SessionLogEntry, session_reconstruct::apply_log_mutations_nats};

pub(super) struct ApprovalRequest<'a> {
    pub tool_call_id: &'a str,
    pub tool_round_seq: u64,
    pub request_seq: u64,
}

impl ApprovalRequest<'_> {
    /// None means this exact request is still pending and can be retried.
    pub fn outcome(
        &self,
        entries: &[(u64, SessionLogEntry)],
        approved: bool,
    ) -> Result<Option<bool>> {
        let pending = crate::nats_worker::derive_pending_hitl_approvals(entries)?;
        if pending.iter().any(|request| {
            request.tool_call_id == self.tool_call_id
                && (request.tool_round_seq, request.seq) == (self.tool_round_seq, self.request_seq)
        }) {
            return Ok(None);
        }
        let effective = apply_log_mutations_nats(entries)?;
        Ok(Some(self.decision(&effective) == Some(approved)))
    }

    fn decision(&self, effective: &[(u64, SessionLogEntry)]) -> Option<bool> {
        let round = effective.iter().position(|(seq, entry)| {
            *seq == self.tool_round_seq && round_contains(entry, self.tool_call_id)
        })?;
        let entries = round_entries(effective, round);
        let request = entries.iter().position(|(seq, entry)| {
            *seq == self.request_seq && is_request(entry, self.tool_call_id)
        })?;
        decision_after_request(&entries[request + 1..], self.tool_call_id)
    }
}

/// Idempotency before an attempt starts uses the latest round containing this ID.
/// A newer round/request with the same ID hides every older decision for it.
pub(super) fn already_decided(
    entries: &[(u64, SessionLogEntry)],
    tool_call_id: &str,
    approved: bool,
) -> Result<bool> {
    let effective = apply_log_mutations_nats(entries)?;
    let Some(round) = effective
        .iter()
        .rposition(|(_, entry)| round_contains(entry, tool_call_id))
    else {
        return Ok(false);
    };
    let entries = round_entries(&effective, round);
    let Some(request) = entries
        .iter()
        .rposition(|(_, entry)| is_request(entry, tool_call_id))
    else {
        return Ok(false);
    };
    Ok(decision_after_request(&entries[request + 1..], tool_call_id) == Some(approved))
}

fn round_contains(entry: &SessionLogEntry, tool_call_id: &str) -> bool {
    matches!(entry, SessionLogEntry::ToolCalls { calls, .. }
        if calls.iter().any(|call| call.id.as_deref() == Some(tool_call_id)))
}

fn is_request(entry: &SessionLogEntry, tool_call_id: &str) -> bool {
    matches!(entry, SessionLogEntry::HitlApprovalRequested { tool_call_id: id, .. }
        if id == tool_call_id)
}

fn round_entries(entries: &[(u64, SessionLogEntry)], round: usize) -> &[(u64, SessionLogEntry)] {
    let entries = &entries[round + 1..];
    let end = entries
        .iter()
        .position(|(_, entry)| {
            matches!(
                entry,
                SessionLogEntry::ToolCalls { .. }
                    | SessionLogEntry::ToolResults { .. }
                    | SessionLogEntry::Cancel { .. }
                    | SessionLogEntry::Clear
            )
        })
        .unwrap_or(entries.len());
    &entries[..end]
}

fn decision_after_request(entries: &[(u64, SessionLogEntry)], tool_call_id: &str) -> Option<bool> {
    entries
        .iter()
        .take_while(|(_, entry)| !is_request(entry, tool_call_id))
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id: id,
                approved,
                ..
            } if id == tool_call_id => Some(*approved),
            _ => None,
        })
}

#[cfg(test)]
#[path = "hitl_tests.rs"]
mod tests;
