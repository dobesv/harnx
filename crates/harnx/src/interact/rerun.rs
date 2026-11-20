//! Classifying a re-run of an answer whose request is no longer pending.
//!
//! The reply of the first run can be lost after the worker recorded it (or a
//! racing frontend answered first); the re-run then reports `Already applied`
//! or `conflicting` instead of a stale unknown, without submitting anything.

use anyhow::Result;
use harnx_core::interaction::InteractionResponse;
use harnx_runtime::interactions::PendingInteraction;

/// The latest answered request for `interaction_id` and the response that
/// accepted it. `request_seq` pins the request when several share one ID.
pub(super) fn answered_request<'a>(
    history: &'a [(u64, harnx_core::session::SessionLogEntry)],
    interaction_id: &str,
    request_seq: Option<u64>,
) -> Option<(PendingInteraction, &'a InteractionResponse)> {
    use harnx_core::session::SessionLogEntry;

    // The latest request for the ID, or the one pinned by --request-seq.
    let (request_seq, entry) = history.iter().rev().find(|(seq, entry)| match entry {
        SessionLogEntry::InteractionRequested {
            interaction_id: id, ..
        } => id == interaction_id && request_seq.is_none_or(|want| *seq == want),
        _ => false,
    })?;
    let SessionLogEntry::InteractionRequested {
        tool_round_seq,
        tool_call_id,
        ..
    } = entry
    else {
        return None; // unreachable: matched above
    };
    let target = PendingInteraction {
        request_seq: *request_seq,
        tool_round_seq: *tool_round_seq,
        tool_call_id: tool_call_id.clone(),
        interaction_id: interaction_id.to_string(),
        request: harnx_runtime::interactions::interaction_request_from_entry(entry)?,
    };
    // The response that answered exactly that request.
    let recorded = history.iter().rev().find_map(|(_, entry)| match entry {
        SessionLogEntry::InteractionResponse {
            request_seq: answered_seq,
            interaction_id: id,
            response,
            ..
        } if id == interaction_id && *answered_seq == *request_seq => Some(response),
        _ => None,
    })?;
    Some((target, recorded))
}

/// Report the classification of an answered re-run. Nothing is submitted or
/// recorded: the recorded response already answered it.
pub(super) fn report_rerun(interaction_id: &str, identical: bool) -> Result<u8> {
    if identical {
        println!(
            "Already applied: an identical response for '{interaction_id}' was recorded earlier."
        );
        Ok(0)
    } else {
        eprintln!(
            "Rejected: a different response for '{interaction_id}' was already accepted \
             (conflicting). Nothing was recorded."
        );
        Ok(1)
    }
}
