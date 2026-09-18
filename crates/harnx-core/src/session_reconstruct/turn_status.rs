//! The turn-status predicates: which terminator ended a turn, which entries
//! belong to the turn now in progress, and which prompt a `Cancel` stopped.
//!
//! Separated from the state reconstruction next door because the two answer
//! different questions from the same log: reconstruction folds a log into the
//! state a turn resumes from, while these read a single boundary out of it.
use super::after_last;
use crate::session::SessionLogEntry;

fn is_terminator(entry: &SessionLogEntry) -> bool {
    matches!(
        entry,
        SessionLogEntry::TurnEnd { .. }
            | SessionLogEntry::Error { .. }
            | SessionLogEntry::Cancel { .. }
    )
}

/// Sequence of the last TurnEnd, Error or Cancel; 0 when none exists.
pub fn last_terminator_seq(entries: &[(u64, SessionLogEntry)]) -> u64 {
    entries
        .iter()
        .rev()
        .find(|(_, e)| is_terminator(e))
        .map_or(0, |(seq, _)| *seq)
}

/// True when the last TurnEnd/Error/Cancel entry is a Cancel; false when that
/// terminator is a TurnEnd or Error, or when the log has no terminator at all.
pub fn last_terminator_is_cancel(entries: &[(u64, SessionLogEntry)]) -> bool {
    entries
        .iter()
        .rev()
        .find(|(_, e)| is_terminator(e))
        .is_some_and(|(_, e)| matches!(e, SessionLogEntry::Cancel { .. }))
}

/// True when the turn currently in progress is already terminated by a
/// `Cancel`: the last terminator is one, and nothing produced since belongs to
/// a turn that started after it. Queued user `Message`s are the exception —
/// input waiting for the next turn does not start one.
///
/// This is the question a worker aborting its own turn has to ask, and it is
/// narrower than [`last_terminator_is_cancel`]: that predicate scans the whole
/// log and stays true for every later turn, since nothing a wind-up writes is
/// a terminator. A worker that consulted it would silently skip the `Cancel`
/// it owes the turn it just abandoned, leaving that turn's `ToolCalls`
/// unanswered and the log with no terminator at all.
pub fn current_turn_is_cancelled(entries: &[(u64, SessionLogEntry)]) -> bool {
    last_terminator_is_cancel(entries)
        && current_turn_entries(entries)
            .iter()
            .all(|(_, entry)| is_queued_user_message(entry))
}

/// True when a `Cancel` terminated the turn that the prompt at `user_msg_seq`
/// started. A `Cancel` at or below that sequence belongs to an earlier turn:
/// this prompt was typed *after* the interruption, not stopped by it. So does
/// a `Cancel` beyond the `TurnEnd` or `Error` that already ended this prompt's
/// turn, which is why only the first terminator above `user_msg_seq` is read.
///
/// This is the question a follower of one prompt has to ask, and it differs
/// from [`current_turn_is_cancelled`] in the other direction: that predicate
/// describes the turn a worker is running now and stays true while input
/// queued behind the `Cancel` waits for a turn of its own. A follower that
/// consulted it would return "interrupted" the instant its own prompt landed.
pub fn prompt_was_interrupted(entries: &[(u64, SessionLogEntry)], user_msg_seq: u64) -> bool {
    prompt_interrupted_at(entries, user_msg_seq).is_some()
}

/// The sequence of the `Cancel` that answers [`prompt_was_interrupted`] for
/// this prompt, so a follower can fence live output by it (`LiveEventState`)
/// instead of only learning that the turn stopped.
pub fn prompt_interrupted_at(entries: &[(u64, SessionLogEntry)], user_msg_seq: u64) -> Option<u64> {
    let (seq, entry) = entries
        .iter()
        .filter(|(seq, _)| *seq > user_msg_seq)
        .find(|(_, entry)| is_terminator(entry))?;
    matches!(entry, SessionLogEntry::Cancel { .. }).then_some(*seq)
}

fn is_queued_user_message(entry: &SessionLogEntry) -> bool {
    matches!(entry, SessionLogEntry::Message { role, .. } if role.is_user())
}

/// Entries strictly after the last terminator.
pub fn current_turn_entries(entries: &[(u64, SessionLogEntry)]) -> &[(u64, SessionLogEntry)] {
    &entries[after_last(entries, |(_, entry)| is_terminator(entry))..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{MessageContent, MessageRole};
    use crate::session_reconstruct::{
        cancel_after_orphan_tool_call, reconstruct_state_from_nats, tool_result_ok,
        OrphanToolCalls, TurnStatus,
    };
    use crate::tool::ToolCall;

    /// A user message. None of this module's assertions inspect message
    /// text, only its position/seq, so callers never need distinct text.
    fn user() -> SessionLogEntry {
        SessionLogEntry::Message {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text("go".to_string()),
            timestamp: None,
            fence_token: None,
        }
    }
    /// A single ToolCalls entry issuing one call, id `"c1"`.
    fn calls() -> SessionLogEntry {
        calls_many(&["c1"])
    }
    /// A single ToolCalls entry issuing one call per id, for multi-call rounds.
    fn calls_many(ids: &[&str]) -> SessionLogEntry {
        SessionLogEntry::ToolCalls {
            text: String::new(),
            thought: None,
            calls: ids
                .iter()
                .map(|id| ToolCall {
                    name: "bash".into(),
                    arguments: serde_json::json!({}),
                    id: Some((*id).into()),
                    thought_signature: None,
                    reasoning_provenance: None,
                })
                .collect(),
            timestamp: None,
            fence_token: None,
        }
    }
    /// A `ToolResults` entry answering call id `"c1"` (matching `calls()`).
    fn results() -> SessionLogEntry {
        tool_result_ok()
    }
    fn cancel(id: &str) -> SessionLogEntry {
        SessionLogEntry::cancel_request(id.into(), "tui:t".into())
    }
    /// Number `entries` from 1 the way the session log does, so the cases
    /// below only have to say which entries they contain and in what order.
    fn numbered(entries: Vec<SessionLogEntry>) -> Vec<(u64, SessionLogEntry)> {
        entries
            .into_iter()
            .zip(1..)
            .map(|(entry, seq)| (seq, entry))
            .collect()
    }
    /// Reconstruct `entries` and assert the session came to rest with `queued`
    /// messages waiting for a turn of their own.
    fn assert_idle_with_queued(entries: Vec<SessionLogEntry>, queued: usize) {
        let state = reconstruct_state_from_nats(&numbered(entries));
        assert_eq!(state.turn_status, TurnStatus::Idle);
        assert_eq!(state.next_turn_messages.len(), queued);
    }
    fn turn_end(through_seq: u64) -> SessionLogEntry {
        SessionLogEntry::TurnEnd {
            through_seq,
            fence_token: 0,
            timestamp: None,
            usage: None,
        }
    }
    /// Expected orphan for a `ToolCalls` entry at `seq` with the given call
    /// ids, built the same way `calls`/`calls_many` build the entry itself.
    /// `OrphanToolCalls`'s derived `PartialEq` lets callers assert the whole
    /// value in one comparison instead of checking `seq`/`calls` separately.
    fn orphan(seq: u64, ids: &[&str]) -> OrphanToolCalls {
        let SessionLogEntry::ToolCalls { calls, .. } = calls_many(ids) else {
            unreachable!("calls_many always builds a ToolCalls entry")
        };
        OrphanToolCalls { seq, calls }
    }

    #[test]
    fn cancel_after_orphan_calls_needs_wind_up() {
        let log = cancel_after_orphan_tool_call(false);
        let state = reconstruct_state_from_nats(&log);
        assert_eq!(
            state.turn_status,
            TurnStatus::InterruptedPendingWindUp {
                cancel_seq: 3,
                cancellation_id: Some("x".to_string()),
                orphans: vec![orphan(2, &["c1"])],
            }
        );
        assert!(state.next_turn_messages.is_empty());
    }

    #[test]
    fn wind_up_results_after_cancel_make_the_session_idle() {
        assert_idle_with_queued(vec![user(), calls(), cancel("x"), results()], 0);
    }

    #[test]
    fn cancel_without_orphans_is_idle_and_closes_queued_messages() {
        assert_idle_with_queued(vec![user(), calls(), results(), user(), cancel("x")], 0);
    }

    #[test]
    fn user_message_after_cancel_starts_a_new_turn() {
        assert_idle_with_queued(vec![user(), calls(), cancel("x"), results(), user()], 1);
    }

    #[test]
    fn orphan_calls_without_cancel_are_resumable() {
        let log = vec![(1, user()), (2, calls())];
        match reconstruct_state_from_nats(&log).turn_status {
            TurnStatus::InFlightResumable { orphans } => assert_eq!(orphans[0].seq, 2),
            other => panic!("expected resumable, got {other:?}"),
        }
    }

    #[test]
    fn current_turn_helpers_split_at_last_terminator() {
        let log = vec![(1, user()), (2, cancel("x")), (3, user()), (4, calls())];
        assert_eq!(last_terminator_seq(&log), 2);
        let turn = current_turn_entries(&log);
        assert_eq!(turn.first().map(|(seq, _)| *seq), Some(3));
        assert_eq!(turn.len(), 2);
    }

    #[test]
    fn last_terminator_is_cancel_reflects_the_final_terminator() {
        let cancelled = vec![(1, user()), (2, cancel("x"))];
        assert!(last_terminator_is_cancel(&cancelled));

        let ended = vec![(1, user()), (2, turn_end(1))];
        assert!(!last_terminator_is_cancel(&ended));

        let no_terminator = vec![(1, user())];
        assert!(!last_terminator_is_cancel(&no_terminator));
    }

    /// The narrower question a worker asks before deciding whether it still
    /// owes the turn it abandoned a `Cancel` of its own.
    #[test]
    fn current_turn_is_cancelled_only_while_nothing_new_has_run() {
        let cancelled = vec![(1, user()), (2, calls()), (3, cancel("x"))];
        assert!(current_turn_is_cancelled(&cancelled));

        // Input queued behind the cancelled turn has not started one.
        let queued = vec![(1, user()), (2, cancel("x")), (3, user())];
        assert!(current_turn_is_cancelled(&queued));

        // A turn that started after the Cancel owes a terminator of its own,
        // even though the last terminator in the log is still that Cancel.
        let next_turn = vec![
            (1, user()),
            (2, cancel("x")),
            (3, results()),
            (4, user()),
            (5, calls()),
        ];
        assert!(last_terminator_is_cancel(&next_turn));
        assert!(!current_turn_is_cancelled(&next_turn));

        let ended = vec![(1, user()), (2, turn_end(1))];
        assert!(!current_turn_is_cancelled(&ended));

        let no_terminator = vec![(1, user()), (2, calls())];
        assert!(!current_turn_is_cancelled(&no_terminator));
    }

    /// The question a follower of one prompt asks, which is the opposite way
    /// round: a `Cancel` that predates the prompt stopped an earlier turn, and
    /// `current_turn_is_cancelled` would wrongly claim that prompt too.
    #[test]
    fn a_prompt_is_only_interrupted_by_a_cancel_that_follows_it() {
        let queued_behind = vec![(1, user()), (2, cancel("x")), (3, user())];
        assert!(current_turn_is_cancelled(&queued_behind));
        assert!(!prompt_was_interrupted(&queued_behind, 3));

        let mut stopped = queued_behind.clone();
        stopped.push((4, cancel("y")));
        assert!(prompt_was_interrupted(&stopped, 3));

        let finished = vec![(1, user()), (2, turn_end(1))];
        assert!(!prompt_was_interrupted(&finished, 1));
    }

    /// Only the first terminator above the prompt answers for it. Once a
    /// `TurnEnd` has closed the prompt's turn, a `Cancel` that stops some
    /// later turn says nothing about this one.
    #[test]
    fn a_cancel_beyond_the_prompts_own_turn_end_does_not_interrupt_it() {
        let log = numbered(vec![user(), turn_end(1), user(), calls(), cancel("x")]);
        assert!(!prompt_was_interrupted(&log, 1));
        // The later prompt is the one that Cancel actually stopped.
        assert!(prompt_was_interrupted(&log, 3));
    }

    /// A user message that arrives before the interrupted turn's tool calls
    /// have all been wound up must not clear the pending wind-up: the worker
    /// still owes the orphaned call a result. `next_turn_messages` keeps
    /// accumulating so the steering message is not lost once wind-up finishes.
    #[test]
    fn user_message_after_cancel_before_wind_up_keeps_wind_up_pending() {
        let log = cancel_after_orphan_tool_call(true);
        let state = reconstruct_state_from_nats(&log);
        match state.turn_status {
            TurnStatus::InterruptedPendingWindUp {
                cancel_seq,
                orphans,
                ..
            } => {
                assert_eq!(cancel_seq, 3);
                assert_eq!(orphans.len(), 1);
                assert_eq!(orphans[0].seq, 2);
            }
            other => panic!("expected wind-up, got {other:?}"),
        }
        assert_eq!(state.next_turn_messages.len(), 1);
    }

    /// A second Cancel with nothing in between (no new ToolCalls/ToolResults)
    /// must not forget the orphan the first Cancel already recorded: only the
    /// cancel identity (seq, cancellation_id) updates to the latest.
    #[test]
    fn second_cancel_keeps_orphans_from_the_first() {
        let mut log = cancel_after_orphan_tool_call(false);
        log.push((
            4,
            SessionLogEntry::cancel_request("y".to_string(), "tui:t".to_string()),
        ));
        let state = reconstruct_state_from_nats(&log);
        assert_eq!(
            state.turn_status,
            TurnStatus::InterruptedPendingWindUp {
                cancel_seq: 4,
                cancellation_id: Some("y".to_string()),
                orphans: vec![orphan(2, &["c1"])],
            }
        );
    }

    /// A ToolResults that answers only some calls of a multi-call round must
    /// not drop the still-unanswered calls: the orphan entry survives with
    /// just those calls remaining.
    #[test]
    fn partial_results_keep_the_unanswered_call_pending() {
        let log = vec![
            (1, user()),
            (2, calls_many(&["c1", "c2"])),
            (3, cancel("x")),
            (4, results()),
        ];
        let state = reconstruct_state_from_nats(&log);
        match state.turn_status {
            TurnStatus::InterruptedPendingWindUp { orphans, .. } => {
                assert_eq!(orphans, vec![orphan(2, &["c2"])]);
            }
            other => panic!("expected wind-up, got {other:?}"),
        }
    }
}
