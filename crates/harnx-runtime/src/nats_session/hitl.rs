//! Recover decision acknowledgements from the original request, not a reused tool ID.
mod receipts;
use anyhow::Result;
use harnx_core::{session::SessionLogEntry, session_reconstruct::apply_log_mutations_nats};
use receipts::last_decision_receipt;
use serde::{Deserialize, Serialize};

use super::{fixed_admission::FixedAdmissionTicket, NatsSession, NatsTurnResult};

/// An unresolved approval, joined to its persisted `ToolCalls` round.
/// Arguments reflect the transcript, not any unpersisted hook mutations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingHitlApproval {
    pub request_seq: u64,
    pub tool_round_seq: u64,
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
    pub summary: String,
}

impl PendingHitlApproval {
    /// Bind this exact approval generation to its invocation for durable replies.
    pub fn approval_ref(&self, invocation_id: impl Into<String>) -> ApprovalRef {
        ApprovalRef {
            invocation_id: invocation_id.into(),
            tool_round_seq: self.tool_round_seq,
            request_seq: self.request_seq,
            tool_call_id: self.tool_call_id.clone(),
        }
    }
}

/// Durable identity of one approval request. A provider's tool call ID alone
/// isn't unique across rounds or invocations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRef {
    pub invocation_id: String,
    pub tool_round_seq: u64,
    pub request_seq: u64,
    pub tool_call_id: String,
}

/// Last decision observed for an invocation, including its exact request identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HitlDecisionReceipt {
    pub request: ApprovalRef,
    pub decision_seq: u64,
    pub approved: bool,
    pub note: Option<String>,
}

/// Result of deciding an exact approval generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HitlDecisionOutcome {
    /// Request was pending at entry; a matching durable answer was observed after delivery.
    Applied,
    /// Same boolean answer was already durable. Note is the original audit note,
    /// not the retry's note. The non-terminal invocation was reactivated.
    AlreadyApplied {
        approved: bool,
        note: Option<String>,
    },
    /// This exact request has the opposite durable boolean answer.
    Conflicting,
    /// Request identity, invocation, or current generation no longer matches,
    /// or the invocation was already terminal when the call inspected it.
    Stale,
}

/// Durable progress of an admitted fixed invocation. An approval is a suspension,
/// not a terminal result; observing progress doesn't activate the worker.
#[derive(Clone, Debug)]
pub enum PromptProgress {
    Running {
        last_decision: Option<HitlDecisionReceipt>,
    },
    InputRequired(PendingHitlApproval),
    Terminal(NatsTurnResult),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct InvocationProgressScope<'a> {
    pub invocation_id: &'a str,
    pub prompt_id: &'a str,
    pub prompt_sequence: u64,
    pub session_id: &'a str,
}

impl NatsSession {
    pub(crate) fn fixed_prompt_progress_from_entries(
        ticket: &FixedAdmissionTicket,
        prompt_sequence: u64,
        session_id: &str,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<PromptProgress> {
        let scope = InvocationProgressScope {
            invocation_id: ticket.invocation_id(),
            prompt_id: ticket.prompt_id(),
            prompt_sequence,
            session_id,
        };
        Self::invocation_progress_from_entries(&scope, entries)
    }

    pub(super) fn invocation_progress_from_entries(
        scope: &InvocationProgressScope<'_>,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<PromptProgress> {
        // Fixed admission excludes a new invocation until this one has a raw
        // terminator. Check that proof before looking at any later approval.
        if let Some(terminal) = super::invocation_terminal_seq(entries, scope.prompt_sequence) {
            return Ok(terminal_progress(scope, entries, terminal));
        }
        if let Some(pending) = pending_approval_for_invocation(entries, scope.prompt_sequence)? {
            return Ok(PromptProgress::InputRequired(pending));
        }
        let last_decision =
            last_decision_receipt(scope.invocation_id, scope.prompt_sequence, entries)?;
        Ok(PromptProgress::Running { last_decision })
    }
}

fn terminal_progress(
    scope: &InvocationProgressScope<'_>,
    entries: &[(u64, SessionLogEntry)],
    terminal: u64,
) -> PromptProgress {
    let (response, error) = NatsSession::extract_turn_outcome(entries, scope.prompt_sequence);
    PromptProgress::Terminal(NatsTurnResult {
        response,
        error,
        session_id: scope.session_id.into(),
        user_msg_id: scope.prompt_id.into(),
        user_msg_seq: scope.prompt_sequence,
        was_cancelled: entries.iter().any(|(seq, entry)| {
            *seq == terminal && matches!(entry, SessionLogEntry::Cancel { .. })
        }),
    })
}

fn pending_approval_for_invocation(
    entries: &[(u64, SessionLogEntry)],
    prompt_sequence: u64,
) -> Result<Option<PendingHitlApproval>> {
    Ok(crate::nats_worker::derive_pending_hitl_approvals(entries)?
        .into_iter()
        // A request written after the prompt can still belong to a round
        // that started before it. Both ends must belong to this invocation.
        .find(|request| request.tool_round_seq > prompt_sequence))
}

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
                && (request.tool_round_seq, request.request_seq)
                    == (self.tool_round_seq, self.request_seq)
        }) {
            return Ok(None);
        }
        let effective = apply_log_mutations_nats(entries)?;
        Ok(Some(self.decision(&effective) == Some(approved)))
    }

    pub(super) fn decision_with_note(
        &self,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<Option<(bool, Option<String>)>> {
        let effective = apply_log_mutations_nats(entries)?;
        Ok(self
            .decision_entry(&effective)
            .map(|(approved, note)| (approved, note.clone())))
    }

    fn decision(&self, effective: &[(u64, SessionLogEntry)]) -> Option<bool> {
        self.decision_entry(effective).map(|(approved, _)| approved)
    }

    fn decision_entry<'a>(
        &self,
        effective: &'a [(u64, SessionLogEntry)],
    ) -> Option<(bool, &'a Option<String>)> {
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
    Ok(
        decision_after_request(&entries[request + 1..], tool_call_id)
            .is_some_and(|(decided, _)| decided == approved),
    )
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

fn decision_after_request<'a>(
    entries: &'a [(u64, SessionLogEntry)],
    tool_call_id: &str,
) -> Option<(bool, &'a Option<String>)> {
    entries
        .iter()
        .take_while(|(_, entry)| !is_request(entry, tool_call_id))
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id: id,
                approved,
                note,
                ..
            } if id == tool_call_id => Some((*approved, note)),
            _ => None,
        })
}

impl<'a> From<&'a ApprovalRef> for ApprovalRequest<'a> {
    fn from(reference: &'a ApprovalRef) -> Self {
        Self {
            tool_call_id: &reference.tool_call_id,
            tool_round_seq: reference.tool_round_seq,
            request_seq: reference.request_seq,
        }
    }
}

pub(super) fn latest_request<'a>(
    entries: &[(u64, SessionLogEntry)],
    tool_call_id: &'a str,
) -> Result<Option<ApprovalRequest<'a>>> {
    let effective = apply_log_mutations_nats(entries)?;
    let Some(round) = effective
        .iter()
        .rposition(|(_, entry)| round_contains(entry, tool_call_id))
    else {
        return Ok(None);
    };
    Ok(round_entries(&effective, round)
        .iter()
        .rev()
        .find_map(|(seq, entry)| {
            is_request(entry, tool_call_id).then_some(ApprovalRequest {
                tool_call_id,
                tool_round_seq: effective[round].0,
                request_seq: *seq,
            })
        }))
}

#[cfg(test)]
#[path = "hitl_tests.rs"]
mod tests;
