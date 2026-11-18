//! Only completed live suspension boundaries bypass the old-reader guard.
//! Incomplete rounds stay parked until crash classification is installed.
use harnx_core::session::SessionLogEntry;

pub(crate) fn live_resume_owned(entries: &[(u64, SessionLogEntry)]) -> bool {
    let Some((round, calls)) = entries.iter().rev().find_map(|(seq, entry)| match entry {
        SessionLogEntry::ToolCalls { calls, .. } => Some((*seq, calls)),
        _ => None,
    }) else {
        return false;
    };
    let tail: Vec<_> = entries.iter().filter(|(seq, _)| *seq > round).collect();
    if tail.iter().any(|(_, e)| {
        matches!(
            e,
            SessionLogEntry::ToolResults { .. }
                | SessionLogEntry::Cancel { .. }
                | SessionLogEntry::Clear
                | SessionLogEntry::Error { .. }
                | SessionLogEntry::TurnEnd { .. }
        )
    }) {
        return false;
    }
    let Some(pending) = tail.iter().rev().find_map(|(_, e)| match e {
        SessionLogEntry::SuspensionReady {
            tool_round_seq,
            pending,
            ..
        } if *tool_round_seq == round => Some(pending),
        _ => None,
    }) else {
        return false;
    };
    !pending.is_empty()
        && calls.iter().all(|call| {
            tail.iter().any(|(_, e)| match e {
                SessionLogEntry::ToolCallCompleted {
                    tool_round_seq,
                    tool_call_id,
                    ..
                } if *tool_round_seq == round && call.id.as_deref() == Some(tool_call_id) => true,
                SessionLogEntry::InteractionRequested {
                    tool_round_seq,
                    tool_call_id,
                    interaction_id,
                    ..
                } if *tool_round_seq == round && call.id.as_deref() == Some(tool_call_id) => {
                    pending.contains(interaction_id)
                }
                _ => false,
            })
        })
}
