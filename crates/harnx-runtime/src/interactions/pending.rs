//! Derivation of pending interactions from durable session log.

use anyhow::Result;
use harnx_core::{
    interaction::InteractionRequest, session::SessionLogEntry,
    session_reconstruct::apply_log_mutations_nats,
};
use serde::{Deserialize, Serialize};

/// One pending interaction requiring user decision, derived from durable session log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingInteraction {
    pub request_seq: u64,
    pub tool_round_seq: u64,
    pub tool_call_id: String,
    pub interaction_id: String,
    pub request: InteractionRequest,
}

impl PendingInteraction {
    pub fn request(&self) -> &InteractionRequest {
        &self.request
    }

    pub fn summary(&self) -> &str {
        &self.request.summary
    }

    pub fn producer(&self) -> &str {
        &self.request.producer
    }
}

/// Derive all currently pending interactions from the session log.
///
/// Gated by the latest `SuspensionReady` in each open round. An interaction is only
/// visible when listed in that round's latest `SuspensionReady.pending`. Accepted
/// responses clear the interaction; round completion (`ToolResults`) or cancellation
/// clears the whole round.
pub fn derive_pending_interactions(
    entries: &[(u64, SessionLogEntry)],
) -> Result<Vec<PendingInteraction>> {
    let effective = apply_log_mutations_nats(entries)?;
    let mut pending = Vec::new();
    for round_seq in open_rounds(&effective) {
        let round_entries = entries_for_round(&effective, round_seq);
        let Some(manifest) = latest_suspension_ready(round_entries, round_seq) else {
            continue;
        };
        for interaction_id in manifest {
            if has_interaction_response(round_entries, interaction_id) {
                continue;
            }
            if let Some(req) = find_interaction_request(round_entries, round_seq, interaction_id) {
                pending.push(req);
            }
        }
    }
    Ok(pending)
}

/// Reconstruct an `InteractionRequest` from a log entry, if it is `InteractionRequested`.
pub fn interaction_request_from_entry(entry: &SessionLogEntry) -> Option<InteractionRequest> {
    match entry {
        SessionLogEntry::InteractionRequested {
            interaction_id,
            producer,
            step_path,
            key,
            kind,
            payload,
            summary,
            chain_version,
            ..
        } => Some(InteractionRequest {
            interaction_id: interaction_id.clone(),
            producer: producer.clone(),
            step_path: step_path.clone(),
            key: key.clone(),
            kind: kind.clone(),
            payload: payload.clone(),
            summary: summary.clone(),
            chain_version: chain_version.clone(),
        }),
        _ => None,
    }
}

pub(crate) fn open_rounds(entries: &[(u64, SessionLogEntry)]) -> Vec<u64> {
    let mut open = Vec::new();
    let mut current = None;
    for (seq, entry) in entries {
        match entry {
            SessionLogEntry::ToolCalls { .. } => {
                if let Some(prev) = current.replace(*seq) {
                    open.push(prev);
                }
            }
            SessionLogEntry::ToolResults { .. } => current = None,
            SessionLogEntry::Cancel { .. } | SessionLogEntry::Clear => {
                open.clear();
                current = None;
            }
            _ => {}
        }
    }
    open.extend(current);
    open
}

pub(crate) fn entries_for_round(
    entries: &[(u64, SessionLogEntry)],
    round_seq: u64,
) -> &[(u64, SessionLogEntry)] {
    let start = entries.partition_point(|(seq, _)| *seq <= round_seq);
    let end = entries[start..]
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
        .map_or(entries.len(), |offset| start + offset);
    &entries[start..end]
}

pub(crate) fn latest_suspension_ready(
    round_entries: &[(u64, SessionLogEntry)],
    round_seq: u64,
) -> Option<&[String]> {
    round_entries
        .iter()
        .rev()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::SuspensionReady {
                tool_round_seq,
                pending,
                ..
            } if *tool_round_seq == round_seq => Some(pending.as_slice()),
            _ => None,
        })
}

pub(crate) fn has_interaction_response(
    round_entries: &[(u64, SessionLogEntry)],
    interaction_id: &str,
) -> bool {
    round_entries.iter().any(|(_, entry)| match entry {
        SessionLogEntry::InteractionResponse {
            interaction_id: id, ..
        } => id == interaction_id,
        _ => false,
    })
}

pub(crate) fn find_interaction_request(
    round_entries: &[(u64, SessionLogEntry)],
    round_seq: u64,
    interaction_id: &str,
) -> Option<PendingInteraction> {
    round_entries.iter().find_map(|(seq, entry)| {
        if let SessionLogEntry::InteractionRequested {
            tool_round_seq,
            tool_call_id,
            interaction_id: id,
            ..
        } = entry
        {
            if *tool_round_seq == round_seq && id == interaction_id {
                let request = interaction_request_from_entry(entry)?;
                return Some(PendingInteraction {
                    request_seq: *seq,
                    tool_round_seq: *tool_round_seq,
                    tool_call_id: tool_call_id.clone(),
                    interaction_id: id.clone(),
                    request,
                });
            }
        }
        None
    })
}

pub(crate) fn find_request_by_id(
    entries: &[(u64, SessionLogEntry)],
    interaction_id: &str,
) -> Option<PendingInteraction> {
    entries.iter().rev().find_map(|(seq, entry)| {
        if let SessionLogEntry::InteractionRequested {
            tool_round_seq,
            tool_call_id,
            interaction_id: id,
            ..
        } = entry
        {
            if id == interaction_id {
                let request = interaction_request_from_entry(entry)?;
                return Some(PendingInteraction {
                    request_seq: *seq,
                    tool_round_seq: *tool_round_seq,
                    tool_call_id: tool_call_id.clone(),
                    interaction_id: id.clone(),
                    request,
                });
            }
        }
        None
    })
}

pub(crate) fn round_is_terminal(entries: &[(u64, SessionLogEntry)], round_seq: u64) -> bool {
    entries.iter().any(|(seq, entry)| {
        *seq > round_seq
            && matches!(
                entry,
                SessionLogEntry::ToolResults { .. }
                    | SessionLogEntry::Cancel { .. }
                    | SessionLogEntry::Clear
                    | SessionLogEntry::TurnEnd { .. }
            )
    })
}

pub(crate) fn existing_response(
    round_entries: &[(u64, SessionLogEntry)],
    interaction_id: &str,
    request_seq: u64,
) -> Option<harnx_core::interaction::InteractionResponse> {
    round_entries.iter().find_map(|(_, entry)| match entry {
        SessionLogEntry::InteractionResponse {
            interaction_id: id,
            request_seq: req_seq,
            response,
            ..
        } if id == interaction_id && *req_seq == request_seq => Some(response.clone()),
        _ => None,
    })
}
