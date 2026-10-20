use crate::session::{CompactOutcome, SessionLogEntry};

pub(super) fn proven_count(compaction_seq: u64, after: &[(u64, SessionLogEntry)]) -> Option<usize> {
    // A receipt immediately after Compress proves no messages were re-logged.
    // EOF doesn't: the reader may have caught compaction between appends.
    // For nonempty spans, even stable IDs aren't universal provenance. Client
    // admissions can interleave, and live kept messages can lose their IDs.
    let (seq, entry) = after.first()?;
    if compaction_seq.checked_add(1) != Some(*seq) {
        return None;
    }
    match entry {
        SessionLogEntry::CompactResult {
            outcome: CompactOutcome::Compacted,
            ..
        }
        | SessionLogEntry::TurnEnd { .. } => Some(0),
        _ => None,
    }
}
