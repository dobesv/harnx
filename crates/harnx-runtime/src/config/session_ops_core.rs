//! Pure session-operation decision helpers shared by local and remote adapters.
use super::*;
use harnx_core::session_reconstruct::active_context_window;

/// Keep a `ToolCalls`+`ToolResults` pair together, or the calls orphan on
/// replay. The expansion skips `HitlApprovalRequested`/`HitlApprovalDecision`
/// entries journaled between the pair (a HITL-gated round writes them while
/// the tool waits for the user), so deleting a tool round removes the whole
/// round — approval receipts and results included (#2557 review).
pub(crate) fn adjust_range_for_tool_pairs(
    from: usize,
    to: usize,
    documents: &[String],
) -> Result<(usize, usize)> {
    let parse = |idx: usize| -> Option<SessionLogEntry> {
        documents
            .get(idx)
            .and_then(|raw| serde_yaml::from_str::<SessionLogEntry>(raw).ok())
    };

    // A range must not start inside a tool round: the round's `ToolCalls`
    // sits before `from`, so the deletion would orphan the calls (results)
    // or split an approval from its round.
    match parse(from) {
        Some(SessionLogEntry::ToolResults { .. }) => bail!(
            "Sequence {from} is a tool-results entry; its paired tool-calls entry \
             would be outside the range. Expand your range to include it."
        ),
        Some(SessionLogEntry::HitlApprovalRequested { .. })
        | Some(SessionLogEntry::HitlApprovalDecision { .. }) => {
            if let Some(calls_idx) = approval_round_calls_index(parse, from) {
                bail!(
                    "Sequence {from} is an approval entry of the tool round at \
                     {calls_idx}; starting the range here would orphan the round's \
                     tool-calls entry. Expand your range to include the whole round."
                );
            }
        }
        _ => {}
    }

    // A range ending on the round's calls or one of its approvals extends to
    // the paired results, so the pair is never split.
    let mut adjusted_to = to;
    match parse(to) {
        Some(SessionLogEntry::ToolCalls { .. }) => {
            if let Some(results_idx) = paired_tool_results_index(parse, to) {
                adjusted_to = results_idx;
            }
        }
        Some(SessionLogEntry::HitlApprovalRequested { .. })
        | Some(SessionLogEntry::HitlApprovalDecision { .. }) => {
            if let Some(calls_idx) = approval_round_calls_index(parse, to) {
                if let Some(results_idx) = paired_tool_results_index(parse, calls_idx) {
                    adjusted_to = results_idx;
                }
            }
        }
        _ => {}
    }

    Ok((from, adjusted_to))
}

pub(crate) fn validate_not_deleting_protected(
    entries: &[SessionLogEntry],
    from: usize,
) -> Result<()> {
    if matches!(entries.get(from), Some(SessionLogEntry::Compress { .. })) {
        bail!("Cannot delete protected session history at or before most-recent boundary");
    }

    let indexed: Vec<_> = entries.iter().cloned().enumerate().collect();
    let window = active_context_window(&indexed);
    if window
        .boundary_index()
        .is_some_and(|boundary| from <= boundary)
    {
        bail!("Cannot delete protected session history at or before most-recent boundary");
    }

    Ok(())
}

pub(crate) fn compute_delete_range(
    from: usize,
    to: usize,
    entries: &[SessionLogEntry],
    documents: &[String],
) -> Result<(usize, usize)> {
    validate_not_deleting_protected(entries, from)?;
    if to >= documents.len() {
        bail!("Sequence numbers out of range");
    }
    let (from, to) = adjust_range_for_tool_pairs(from, to, documents)?;
    if from > to || to >= documents.len() {
        bail!("Sequence numbers out of range");
    }
    Ok((from, to))
}

/// Index of the `ToolResults` entry paired with the `ToolCalls` entry at
/// `calls_idx`, skipping approval control entries written between the pair.
///
/// A HITL-gated tool round journals `HitlApprovalRequested`/`HitlApprovalDecision`
/// after its `ToolCalls` and before its `ToolResults` while the tool waits for
/// the user, so strict adjacency (`+ 1`) misses the pair.
fn paired_tool_results_index(
    parse: impl Fn(usize) -> Option<SessionLogEntry>,
    calls_idx: usize,
) -> Option<usize> {
    let mut idx = calls_idx + 1;
    while let Some(entry) = parse(idx) {
        match entry {
            SessionLogEntry::ToolResults { .. } => return Some(idx),
            SessionLogEntry::HitlApprovalRequested { .. }
            | SessionLogEntry::HitlApprovalDecision { .. } => idx += 1,
            _ => return None,
        }
    }
    None
}

/// Index of the `ToolCalls` entry owning the approval entry at `idx`, when
/// only approval entries separate the two. `None` when `idx` is not an
/// approval entry or its round's calls are missing/further away.
fn approval_round_calls_index(
    parse: impl Fn(usize) -> Option<SessionLogEntry>,
    idx: usize,
) -> Option<usize> {
    let mut i = idx;
    loop {
        match parse(i)? {
            SessionLogEntry::HitlApprovalRequested { .. }
            | SessionLogEntry::HitlApprovalDecision { .. } => i = i.checked_sub(1)?,
            SessionLogEntry::ToolCalls { .. } => return Some(i),
            _ => return None,
        }
    }
}

pub(crate) fn compute_rewind_point(
    after_seq: usize,
    log_entry_count: usize,
    entries: &[SessionLogEntry],
) -> Result<usize> {
    if after_seq >= log_entry_count {
        bail!(
            "Sequence number {} is out of range (log has {} entries)",
            after_seq,
            log_entry_count
        );
    }

    let indexed: Vec<_> = entries.iter().cloned().enumerate().collect();
    let window = active_context_window(&indexed);
    if window
        .boundary_index()
        .is_some_and(|boundary| after_seq <= boundary)
    {
        bail!("Cannot rewind to or before most-recent boundary");
    }

    let parse = |idx: usize| entries.get(idx).cloned();
    // Rewinding to a tool-calls entry must keep its paired tool-results, or
    // the calls orphan on replay. Expand the rewind point to include the
    // pair, so `.rewind <n>` on a tool round keeps the whole round and drops
    // only what follows it (issue #2557). Delete enforces the same
    // keep-the-pair rule via `adjust_range_for_tool_pairs`, skipping the
    // approval entries journaled between the pair.
    if matches!(parse(after_seq), Some(SessionLogEntry::ToolCalls { .. })) {
        if let Some(results_idx) = paired_tool_results_index(parse, after_seq) {
            return Ok(results_idx);
        }
    }

    // Rewinding to an approval entry of a completed round keeps the whole
    // round: dropping just its results orphans the calls, and the next worker
    // activation would resume the approved round — re-executing a tool that
    // already ran. Unanswered rounds have no results to keep.
    if let Some(calls_idx) = approval_round_calls_index(parse, after_seq) {
        if let Some(results_idx) = paired_tool_results_index(parse, calls_idx) {
            return Ok(results_idx);
        }
    }

    Ok(after_seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::message::{MessageContent, MessageRole};

    fn user(text: &str) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: None,
            timestamp: None,
            fence_token: None,
            role: MessageRole::User,
            content: MessageContent::Text(text.into()),
        }
    }

    fn assistant(text: &str) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: None,
            timestamp: None,
            fence_token: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text(text.into()),
        }
    }

    fn compress() -> SessionLogEntry {
        SessionLogEntry::Compress {
            prompt: "summary".into(),
        }
    }

    #[test]
    fn validate_not_deleting_protected_rejects_compress_boundary() {
        let entries = vec![user("old"), compress(), user("new")];
        let err = validate_not_deleting_protected(&entries, 1).expect_err("compress protected");
        assert_eq!(
            err.to_string(),
            "Cannot delete protected session history at or before most-recent boundary"
        );
    }

    #[test]
    fn validate_not_deleting_protected_rejects_pre_boundary_history() {
        let entries = vec![user("old"), assistant("older"), compress(), user("new")];
        let err = validate_not_deleting_protected(&entries, 0).expect_err("pre-boundary protected");
        assert_eq!(
            err.to_string(),
            "Cannot delete protected session history at or before most-recent boundary"
        );
    }

    #[test]
    fn validate_not_deleting_protected_allows_post_boundary_turn() {
        let entries = vec![user("old"), compress(), user("new"), assistant("reply")];
        validate_not_deleting_protected(&entries, 2).unwrap();
    }

    #[test]
    fn compute_rewind_point_rejects_boundary_or_earlier() {
        let entries = vec![user("old"), compress(), user("new")];
        let err = compute_rewind_point(1, entries.len(), &entries).expect_err("boundary protected");
        assert_eq!(
            err.to_string(),
            "Cannot rewind to or before most-recent boundary"
        );
    }

    fn tool_calls() -> SessionLogEntry {
        SessionLogEntry::ToolCalls {
            text: "checking".into(),
            thought: None,
            calls: vec![],
            timestamp: None,
            fence_token: None,
        }
    }

    fn tool_results() -> SessionLogEntry {
        SessionLogEntry::ToolResults {
            results: vec![],
            timestamp: None,
        }
    }

    /// Completed HITL-gated round: user, calls, request, decision, results,
    /// plus a trailing assistant reply.
    fn hitl_round() -> Vec<SessionLogEntry> {
        vec![
            user("run it"),
            tool_calls(),
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id: "call-1".into(),
                summary: "Approve write".into(),
                fence_token: 1,
            },
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id: "call-1".into(),
                approved: true,
                note: None,
                fence_token: 1,
            },
            tool_results(),
            assistant("done"),
        ]
    }

    fn to_documents(entries: &[SessionLogEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| serde_yaml::to_string(entry).expect("entry serializes"))
            .collect()
    }

    #[test]
    fn compute_rewind_point_keeps_paired_tool_results() {
        // Issue #2557: rewinding to a ToolCalls entry must include its paired
        // ToolResults instead of erroring, mirroring delete's pair expansion.
        let entries = vec![
            user("run it"),
            tool_calls(),
            tool_results(),
            assistant("done"),
        ];
        let after_seq = compute_rewind_point(1, entries.len(), &entries)
            .expect("rewind to a completed tool round must succeed");
        assert_eq!(after_seq, 2, "the paired tool-results entry stays");
    }

    #[test]
    fn compute_rewind_point_leaves_unpaired_trailing_tool_calls_alone() {
        let entries = vec![user("run it"), tool_calls()];
        let after_seq = compute_rewind_point(1, entries.len(), &entries)
            .expect("trailing tool round has nothing to pair with");
        assert_eq!(after_seq, 1);
    }

    #[test]
    fn compute_rewind_point_skips_hitl_entries_between_tool_pair() {
        // A HITL-gated round journals the approval request and decision
        // between the calls and their results, so the pair is not adjacent.
        let entries = [
            user("run it"),
            tool_calls(),
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id: "call-1".into(),
                summary: "Approve write".into(),
                fence_token: 1,
            },
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id: "call-1".into(),
                approved: true,
                note: None,
                fence_token: 1,
            },
            tool_results(),
            assistant("done"),
        ];
        let after_seq = compute_rewind_point(1, entries.len(), &entries)
            .expect("rewind to a HITL-gated tool round must succeed");
        assert_eq!(
            after_seq, 4,
            "the paired tool-results entry stays, skipping the approval entries"
        );
    }

    #[test]
    fn compute_rewind_point_leaves_hitl_gated_unanswered_round_alone() {
        // An approval requested but not yet decided: no results exist, so the
        // rewind point stays on the calls themselves.
        let entries = [
            user("run it"),
            tool_calls(),
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id: "call-1".into(),
                summary: "Approve write".into(),
                fence_token: 1,
            },
        ];
        let after_seq = compute_rewind_point(1, entries.len(), &entries)
            .expect("unanswered tool round has nothing to pair with");
        assert_eq!(after_seq, 1);
    }

    #[test]
    fn adjust_range_for_tool_pairs_deletes_hitl_gated_round_together() {
        // Mirrors compute_rewind_point_skips_hitl_entries_between_tool_pair:
        // deleting the calls must sweep the approval entries and the results
        // into the range, or the round orphans on replay.
        let documents = to_documents(&hitl_round());

        let (from, to) = adjust_range_for_tool_pairs(1, 1, &documents)
            .expect("deleting a HITL-gated tool call must succeed");

        assert_eq!(
            (from, to),
            (1, 4),
            "the approval entries and paired results are deleted with the calls"
        );
    }

    #[test]
    fn adjust_range_for_tool_pairs_still_orphans_on_unanswered_round() {
        // No results exist yet, so deleting the calls cannot extend the
        // range.
        let entries = [
            user("run it"),
            tool_calls(),
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id: "call-1".into(),
                summary: "Approve write".into(),
                fence_token: 1,
            },
        ];
        let documents = to_documents(&entries);

        let (from, to) = adjust_range_for_tool_pairs(1, 1, &documents)
            .expect("deleting the calls of an unanswered round must succeed");

        assert_eq!((from, to), (1, 1));
    }

    #[test]
    fn adjust_range_for_tool_pairs_rejects_range_starting_at_results() {
        let entries = [user("run it"), tool_calls(), tool_results()];
        let documents = to_documents(&entries);

        let err = adjust_range_for_tool_pairs(2, 2, &documents)
            .expect_err("deleting results without the calls orphans the round");
        assert_eq!(
            err.to_string(),
            "Sequence 2 is a tool-results entry; its paired tool-calls entry \
             would be outside the range. Expand your range to include it."
        );
    }

    #[test]
    fn adjust_range_for_tool_pairs_stops_at_next_rounds_calls() {
        // An unanswered round's calls must not pair with the *following*
        // round's results: the scan stops at the next ToolCalls instead of
        // skipping further.
        let entries = [
            user("run it"),
            tool_calls(),
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id: "call-1".into(),
                summary: "Approve write".into(),
                fence_token: 1,
            },
            tool_calls(),
            tool_results(),
        ];
        let documents = to_documents(&entries);

        let (from, to) = adjust_range_for_tool_pairs(1, 1, &documents)
            .expect("deleting the unanswered round's calls must succeed");

        assert_eq!(
            (from, to),
            (1, 1),
            "the next round's calls end the scan; nothing is paired"
        );
    }

    #[test]
    fn adjust_range_for_tool_pairs_does_not_sweep_next_round() {
        // A completed HITL-gated round expands only to its own results, not
        // through the following round.
        let entries = [
            user("run it"),
            tool_calls(),
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id: "call-1".into(),
                summary: "Approve write".into(),
                fence_token: 1,
            },
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id: "call-1".into(),
                approved: true,
                note: None,
                fence_token: 1,
            },
            tool_results(),
            tool_calls(),
            tool_results(),
        ];
        let documents = to_documents(&entries);

        let (from, to) = adjust_range_for_tool_pairs(1, 1, &documents)
            .expect("deleting the first round must succeed");

        assert_eq!(
            (from, to),
            (1, 4),
            "the expansion stops at the round's own results"
        );
    }

    #[test]
    fn adjust_range_for_tool_pairs_keeps_already_expanded_range() {
        // A range that already includes the whole round is passed through
        // unchanged.
        let documents = to_documents(&hitl_round());

        let (from, to) = adjust_range_for_tool_pairs(1, 4, &documents)
            .expect("an already-expanded range must succeed");

        assert_eq!((from, to), (1, 4));
    }

    #[test]
    fn adjust_range_for_tool_pairs_rejects_from_on_approval_entry() {
        // Starting inside the round's approvals would delete them (and the
        // results) while keeping the calls — orphaning the round.
        let documents = to_documents(&hitl_round());

        let err = adjust_range_for_tool_pairs(2, 4, &documents)
            .expect_err("a range must not start on an approval entry of a round");
        assert_eq!(
            err.to_string(),
            "Sequence 2 is an approval entry of the tool round at 1; starting the \
             range here would orphan the round's tool-calls entry. Expand your \
             range to include the whole round."
        );
    }

    #[test]
    fn adjust_range_for_tool_pairs_extends_to_on_approval_entry() {
        // Ending on the round's approvals would delete the calls and
        // approvals while keeping the results — the pair is extended to the
        // results instead.
        let documents = to_documents(&hitl_round());

        let (from, to) = adjust_range_for_tool_pairs(1, 3, &documents)
            .expect("a range ending on an approval entry must succeed");

        assert_eq!((from, to), (1, 4));
    }

    #[test]
    fn compute_rewind_point_keeps_round_when_rewinding_to_approval_entry() {
        // Rewinding to the round's request or decision must keep the whole
        // round; dropping only the results orphans the calls and the worker
        // would resume the approved round, re-running the tool.
        let entries = hitl_round();

        for after_seq in [2, 3] {
            let results_idx = compute_rewind_point(after_seq, entries.len(), &entries)
                .expect("rewinding to a round's approval entry must succeed");
            assert_eq!(
                results_idx, 4,
                "the round stays whole when rewinding to entry {after_seq}"
            );
        }
    }

    #[test]
    fn compute_rewind_point_leaves_unanswered_round_request_alone() {
        // No results exist yet, so rewinding to the pending request keeps
        // the rewind point unchanged.
        let entries = vec![
            user("run it"),
            tool_calls(),
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id: "call-1".into(),
                summary: "Approve write".into(),
                fence_token: 1,
            },
        ];

        let after_seq = compute_rewind_point(2, entries.len(), &entries)
            .expect("rewinding to a pending request must succeed");

        assert_eq!(after_seq, 2);
    }
}
