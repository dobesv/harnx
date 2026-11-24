//! Reader-before-writer rollout guard. Never reconcile a newer worker's effects.

use anyhow::Result;
use async_nats::jetstream;
use harnx_core::{session::SessionLogEntry, session_reconstruct::apply_log_mutations_nats};

// Use the same Cancel/Clear boundaries as the pending-consent reader. Retained
// checkpoint records from abandoned rounds must neither park nor resume work.
use crate::interactions::pending::open_rounds;

pub(crate) fn interaction_record_for_round(entries: &[(u64, SessionLogEntry)], round: u64) -> bool {
    entries.iter().any(|(_, entry)| {
        matches!(entry,
            SessionLogEntry::ToolCallCompleted { tool_round_seq, .. }
            | SessionLogEntry::InteractionRequested { tool_round_seq, .. }
            | SessionLogEntry::SuspensionReady { tool_round_seq, .. }
            if *tool_round_seq == round)
    })
}

/// Only the trusted selection marker plus persisted control metadata can park a
/// handoff. Never rediscover control flow from script-authored output JSON.
pub(crate) fn pending_handoff(entries: &[(u64, SessionLogEntry)]) -> Option<u64> {
    let index = entries
        .iter()
        .rposition(|(_, entry)| matches!(entry, SessionLogEntry::ToolResults { .. }))?;
    let (
        seq,
        SessionLogEntry::ToolResults {
            results,
            handoff_selection: Some(selected),
            ..
        },
    ) = &entries[index]
    else {
        return None;
    };
    let trusted = results.iter().any(|result| {
        result.id.as_ref() == Some(selected)
            && result.switch_agent.is_some()
            && result
                .output
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
            && result
                .output
                .get("isError")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
    });
    let resolved = entries[index + 1..].iter().any(|(_, entry)| {
        matches!(
            entry,
            SessionLogEntry::Cancel { .. } | SessionLogEntry::Clear
        ) || matches!(entry, SessionLogEntry::HandoffCommitted { handoff_tool_call_id, .. }
                if handoff_tool_call_id.as_ref() == Some(selected))
    });
    (trusted && !resolved).then_some(*seq)
}

/// Log-only half for synchronous progress parsers. Storage reads belong in their
/// async wrappers, not in a parser or a blocking runtime callback.
pub(crate) fn log_is_parked(entries: &[(u64, SessionLogEntry)]) -> bool {
    pending_handoff(entries).is_some()
        || open_rounds(entries)
            .into_iter()
            .any(|round| interaction_record_for_round(entries, round))
}

/// Every path that could repair/replay/continue an open round uses this check.
/// Missing is safe only after a successful read; every other store error parks.
pub(crate) async fn is_parked(
    js: &jetstream::Context,
    storage_key: &str,
    entries: &[(u64, SessionLogEntry)],
) -> Result<bool> {
    let effective = apply_log_mutations_nats(entries)?;
    if let Some(seq) = pending_handoff(&effective) {
        warn_parked(storage_key, seq, "selected handoff is not committed");
        return Ok(true);
    }
    for round in open_rounds(&effective) {
        if interaction_record_for_round(&effective, round) {
            warn_parked(storage_key, round, "interaction-owned log records");
            return Ok(true);
        }
        match super::round_started(js, storage_key, round).await {
            Ok(false) => {}
            Ok(true) => {
                warn_parked(storage_key, round, "interaction round-start exists");
                return Ok(true);
            }
            Err(error) => {
                warn_parked(
                    storage_key,
                    round,
                    &format!("checkpoint store read failed: {error:#}"),
                );
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn warn_parked(storage_key: &str, round: u64, reason: &str) {
    log::warn!(
        "session parked: newer worker needed: session_id={storage_key} tool_round_seq={round} reason={reason}"
    );
}

#[cfg(test)]
#[path = "guard_tests.rs"]
mod tests;
