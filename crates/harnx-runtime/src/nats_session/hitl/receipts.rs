//! Track request/decision correlation within each effective tool round.
use super::*;
use std::collections::HashMap;

struct DecisionHistory<'a> {
    invocation_id: &'a str,
    round: Option<(u64, &'a SessionLogEntry)>,
    requests: HashMap<&'a str, (u64, u64)>,
    last_decision: Option<HitlDecisionReceipt>,
}

pub(super) fn last_decision_receipt(
    invocation_id: &str,
    prompt_sequence: u64,
    entries: &[(u64, SessionLogEntry)],
) -> Result<Option<HitlDecisionReceipt>> {
    let effective = apply_log_mutations_nats(entries)?;
    let mut history = DecisionHistory {
        invocation_id,
        round: None,
        requests: HashMap::new(),
        last_decision: None,
    };
    for (seq, entry) in effective.iter().filter(|(seq, _)| *seq > prompt_sequence) {
        history.observe(*seq, entry);
    }
    Ok(history.last_decision)
}

impl<'a> DecisionHistory<'a> {
    fn observe(&mut self, seq: u64, entry: &'a SessionLogEntry) {
        match entry {
            SessionLogEntry::ToolCalls { .. } => {
                self.round = Some((seq, entry));
                self.requests.clear();
            }
            SessionLogEntry::ToolResults { .. }
            | SessionLogEntry::Cancel { .. }
            | SessionLogEntry::Clear => {
                self.round = None;
                self.requests.clear();
            }
            SessionLogEntry::HitlApprovalRequested { tool_call_id, .. } => {
                self.remember_request(seq, tool_call_id);
            }
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id,
                approved,
                note,
                ..
            } => {
                let Some((tool_round_seq, request_seq)) =
                    self.requests.remove(tool_call_id.as_str())
                else {
                    return;
                };
                self.last_decision = Some(HitlDecisionReceipt {
                    request: ApprovalRef {
                        invocation_id: self.invocation_id.into(),
                        tool_round_seq,
                        request_seq,
                        tool_call_id: tool_call_id.clone(),
                    },
                    decision_seq: seq,
                    approved: *approved,
                    note: note.clone(),
                });
            }
            _ => {}
        }
    }

    fn remember_request(&mut self, seq: u64, tool_call_id: &'a str) {
        if let Some((round_seq, _)) = self
            .round
            .filter(|(_, entry)| round_contains(entry, tool_call_id))
        {
            self.requests.insert(tool_call_id, (round_seq, seq));
        }
    }
}
