//! Recover decision acknowledgements from the original request, not a reused tool ID.
use anyhow::Result;
use harnx_core::{session::SessionLogEntry, session_reconstruct::apply_log_mutations_nats};
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

impl NatsSession {
    pub(crate) fn fixed_prompt_progress_from_entries(
        ticket: &FixedAdmissionTicket,
        prompt_sequence: u64,
        session_id: &str,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<PromptProgress> {
        // Fixed admission excludes a new invocation until this one has a raw
        // terminator. Check that proof before looking at any later approval.
        if let Some(terminal) = super::invocation_terminal_seq(entries, prompt_sequence) {
            let (response, error) = Self::extract_turn_outcome(entries, prompt_sequence);
            return Ok(PromptProgress::Terminal(NatsTurnResult {
                response,
                error,
                session_id: session_id.into(),
                user_msg_id: ticket.prompt_id().into(),
                user_msg_seq: prompt_sequence,
                was_cancelled: entries.iter().any(|(seq, entry)| {
                    *seq == terminal && matches!(entry, SessionLogEntry::Cancel { .. })
                }),
            }));
        }
        if let Some(pending) = crate::nats_worker::derive_pending_hitl_approvals(entries)?
            .into_iter()
            // A request written after the prompt can still belong to a round
            // that started before it. Both ends must belong to this invocation.
            .find(|request| request.tool_round_seq > prompt_sequence)
        {
            return Ok(PromptProgress::InputRequired(pending));
        }
        let effective = apply_log_mutations_nats(entries)?;
        let mut round = None;
        let mut requests = std::collections::HashMap::new();
        let mut last_decision = None;
        for (seq, entry) in effective.iter().filter(|(seq, _)| *seq > prompt_sequence) {
            match entry {
                SessionLogEntry::ToolCalls { .. } => {
                    round = Some((*seq, entry));
                    requests.clear();
                }
                SessionLogEntry::ToolResults { .. }
                | SessionLogEntry::Cancel { .. }
                | SessionLogEntry::Clear => {
                    round = None;
                    requests.clear();
                }
                SessionLogEntry::HitlApprovalRequested { tool_call_id, .. } => {
                    if let Some((round_seq, _)) =
                        round.filter(|(_, entry)| round_contains(entry, tool_call_id))
                    {
                        requests.insert(tool_call_id, (round_seq, *seq));
                    }
                }
                SessionLogEntry::HitlApprovalDecision {
                    tool_call_id,
                    approved,
                    note,
                    ..
                } => {
                    if let Some((tool_round_seq, request_seq)) = requests.remove(tool_call_id) {
                        last_decision = Some(HitlDecisionReceipt {
                            request: ApprovalRef {
                                invocation_id: ticket.invocation_id().into(),
                                tool_round_seq,
                                request_seq,
                                tool_call_id: tool_call_id.clone(),
                            },
                            decision_seq: *seq,
                            approved: *approved,
                            note: note.clone(),
                        });
                    }
                }
                _ => {}
            }
        }
        Ok(PromptProgress::Running { last_decision })
    }
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
