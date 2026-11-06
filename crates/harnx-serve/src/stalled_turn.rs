//! Point-in-time detection that a turn has stalled: durable input is pending,
//! no worker holds the lease, and the input has been waiting long enough that
//! no live worker can still be responsible for it.
//!
//! The follower watchdog reaches the same verdict by polling the lease across
//! seconds ([`harnx_runtime::nats_session::SessionLeaseWatchdog`]).
//! `session/get` reads once, so it derives the verdict from what that single
//! read already has: the log entries and the lease sample. Nothing is written
//! back — the log stays the worker's record, and a worker that claims the
//! prompt later turns the state back to running on the next read.
use chrono::{DateTime, Utc};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::nats_session::WORKER_LOST_GRACE;
use std::borrow::Cow;
use std::time::Duration;

pub(crate) use crate::session_actor_types::StallReason;

/// A turn whose prompt nobody is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StalledTurn {
    pub(crate) prompt_seq: u64,
    pub(crate) reason: StallReason,
}

/// One durable read's evidence about a possibly stalled turn: its raw log
/// rows, the lease sample taken alongside them, and the clocks that judge
/// their age. Bundled so the derivation takes the read instead of restating
/// its fields at every call.
pub(crate) struct StallRead<'a> {
    pub(crate) entries: &'a [(u64, SessionLogEntry)],
    /// Whether a worker held the lease across the read.
    pub(crate) worker_active: bool,
    /// The read's resolved `pending_prompt_seq` answer, so the log's
    /// mutations replay once per read.
    pub(crate) prompt_seq: Option<u64>,
    /// When that prompt was admitted, when the read could establish it.
    pub(crate) prompt_admitted_at: Option<DateTime<Utc>>,
    /// The clock the verdict is judged by.
    pub(crate) now: DateTime<Utc>,
    pub(crate) lease_acquisition_timeout: Duration,
}

/// Whether the log's unanswered prompt has waited past every worker deadline.
///
/// Returns `None` (keep reporting running) whenever the read cannot prove the
/// turn stalled: a lease is present, the prompt is young, or the log is too
/// old to timestamp its input.
pub(crate) fn stalled_turn(read: &StallRead<'_>) -> Option<StalledTurn> {
    if read.worker_active {
        return None;
    }
    let prompt_seq = read.prompt_seq?;
    match worker_evidence(read.entries, prompt_seq) {
        // A worker claimed the turn, and the lease sample shows no holder: a
        // released lease deletes its key outright, and a lost one stops
        // renewing, so the worker is gone or between claims. The watchdog
        // tolerates a missing lease for its grace window; age since the
        // worker's last timestamped durable write is how this read measures
        // that same window.
        WorkerEvidence::Timestamped(written_at) => {
            worker_lost_if_aged(written_at, read.now, prompt_seq)
        }
        // The worker wrote, but its last row carries no time. An approval
        // decision is the only worker row that carries no timestamp, and the
        // worker that recorded it may legitimately still be re-claiming the
        // lease to resume — the admission time is not a substitute reference,
        // so a timestampless tail keeps reporting running rather than
        // inventing a verdict.
        WorkerEvidence::Untimestamped => None,
        // No worker ever wrote for the turn. The acquisition timeout is the
        // deadline the follower gives a first claim, measured from admission.
        WorkerEvidence::None => unclaimed_if_past_budget(read, prompt_seq),
    }
}

/// What the turn's worker-written rows prove about the worker's last activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerEvidence {
    /// The worker wrote, and its last row records this time — the reference
    /// the grace ages from.
    Timestamped(DateTime<Utc>),
    /// The worker wrote, but its last row records no time: nothing to age from.
    Untimestamped,
    /// No worker row belongs to the turn.
    None,
}

/// The worker's rows for the turn in progress, reduced to their evidence.
///
/// Edits and rewinds are applied first: an `EditEntries` row can delete the
/// `Error` that ended the turn, re-opening it, and a `Rewind` drops the rows
/// after its cut just as well. Durable `TurnEnd` rows survive every mutation
/// and still bound the turn. The reduction is "last row wins": a timestamped
/// row is the grace reference, and a timestampless one — an approval
/// decision, or a row from before timestamps were stamped at append — clears
/// whatever older write preceded it, so a worker that resumes and writes
/// again re-establishes the reference.
fn worker_evidence(entries: &[(u64, SessionLogEntry)], prompt_seq: u64) -> WorkerEvidence {
    let effective = effective_rows(entries);
    let turn = rows_after_last_editable_terminator(&effective);
    rows_after_the_prompt(turn, prompt_seq)
        .iter()
        .filter(|(seq, _)| !covered_by_durable_turn_end(entries, seq))
        .filter(|(_, entry)| is_worker_written(entry))
        .fold(WorkerEvidence::None, latest_worker_row)
}

/// The turn's rows that belong to the prompt's own attempt: everything after
/// the prompt's position in the replayed stream. A replacement row inherits
/// its edit's sequence, which can exceed a later prompt's sequence, so
/// comparing sequences would read a rewritten earlier-turn row as evidence
/// for the pending prompt.
fn rows_after_the_prompt(
    turn: &[(u64, SessionLogEntry)],
    prompt_seq: u64,
) -> &[(u64, SessionLogEntry)] {
    turn.iter()
        .position(|(seq, _)| *seq == prompt_seq)
        .map_or(&[], |idx| &turn[idx + 1..])
}

fn latest_worker_row(
    _current: WorkerEvidence,
    (_, entry): &(u64, SessionLogEntry),
) -> WorkerEvidence {
    match entry.timestamp() {
        Some(written_at) => WorkerEvidence::Timestamped(written_at),
        None => WorkerEvidence::Untimestamped,
    }
}

fn worker_lost_if_aged(
    written_at: DateTime<Utc>,
    now: DateTime<Utc>,
    prompt_seq: u64,
) -> Option<StalledTurn> {
    // Elapsed time, never `written_at + grace`: the addition can overflow on a
    // corrupted timestamp, and a write dated in the future is young, not lost.
    now.signed_duration_since(written_at)
        .to_std()
        .is_ok_and(|age| age >= WORKER_LOST_GRACE)
        .then_some(StalledTurn {
            prompt_seq,
            reason: StallReason::WorkerLost,
        })
}

fn unclaimed_if_past_budget(read: &StallRead<'_>, prompt_seq: u64) -> Option<StalledTurn> {
    let admitted_at = read.prompt_admitted_at?;
    read.now
        .signed_duration_since(admitted_at)
        .to_std()
        .is_ok_and(|age| age >= read.lease_acquisition_timeout)
        .then_some(StalledTurn {
            prompt_seq,
            reason: StallReason::Unclaimed,
        })
}

/// The log with its mutations applied. Only edits and rewinds change the
/// replayed stream — a log with neither replays to an identical copy, so the
/// raw rows are borrowed instead; a replay that fails reads the raw log, the
/// same fallback `pending_prompt_seq` uses.
fn effective_rows(entries: &[(u64, SessionLogEntry)]) -> Cow<'_, [(u64, SessionLogEntry)]> {
    if !entries.iter().any(|(_, entry)| is_mutation(entry)) {
        return Cow::Borrowed(entries);
    }
    harnx_core::session_reconstruct::apply_log_mutations_nats(entries)
        .map(Cow::Owned)
        .unwrap_or_else(|_| Cow::Borrowed(entries))
}

fn is_mutation(entry: &SessionLogEntry) -> bool {
    matches!(
        entry,
        SessionLogEntry::EditEntries { .. } | SessionLogEntry::Rewind { .. }
    )
}

/// The replayed rows that belong to the turn in progress: everything after
/// the last `Error` or `Cancel`. Unlike the raw log's own turn window, an
/// edit that removes an `Error` re-opens the turn it ended.
fn rows_after_last_editable_terminator(
    effective: &[(u64, SessionLogEntry)],
) -> &[(u64, SessionLogEntry)] {
    let after = effective
        .iter()
        .rposition(|(_, entry)| is_editable_terminator(entry))
        .map_or(0, |idx| idx + 1);
    &effective[after..]
}

fn is_editable_terminator(entry: &SessionLogEntry) -> bool {
    matches!(
        entry,
        SessionLogEntry::Error { .. } | SessionLogEntry::Cancel { .. }
    )
}

/// Whether a durable `TurnEnd` covered this row's turn. `TurnEnd` is control
/// metadata that no edit removes and that mutation replay omits, so the
/// boundary is checked against the raw log — the same rule
/// `pending_prompt_seq` applies to the prompt itself.
fn covered_by_durable_turn_end(raw: &[(u64, SessionLogEntry)], seq: &u64) -> bool {
    raw.iter().any(|(_, entry)| {
        matches!(entry, SessionLogEntry::TurnEnd { through_seq, .. } if *through_seq >= *seq)
    })
}

/// Only the lease-holding worker writes these rows. `ToolResults` carries no
/// fence token of its own but always follows the worker's fenced `ToolCalls`.
fn is_worker_written(entry: &SessionLogEntry) -> bool {
    entry.fence_token().is_some_and(|fence| fence > 0)
        || matches!(entry, SessionLogEntry::ToolResults { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::message::{MessageContent, MessageRole};
    use std::time::Duration;

    const ACQUISITION_TIMEOUT: Duration = Duration::from_secs(60);
    const GRACE: Duration = WORKER_LOST_GRACE;

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn user_prompt(seq: u64) -> (u64, SessionLogEntry) {
        (
            seq,
            SessionLogEntry::Message {
                id: Some("prompt-1".into()),
                role: MessageRole::User,
                content: MessageContent::Text("unanswered".into()),
                timestamp: None,
                fence_token: None,
            },
        )
    }

    fn fenced_write(seq: u64, timestamp: Option<DateTime<Utc>>) -> (u64, SessionLogEntry) {
        (
            seq,
            SessionLogEntry::ToolCalls {
                text: "half a turn".into(),
                thought: None,
                calls: vec![],
                timestamp,
                fence_token: Some(3),
            },
        )
    }

    fn approval_decision(seq: u64) -> (u64, SessionLogEntry) {
        (
            seq,
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id: "derived-call".into(),
                approved: true,
                note: None,
                fence_token: 8,
            },
        )
    }

    fn turn_end(seq: u64, through_seq: u64) -> (u64, SessionLogEntry) {
        (
            seq,
            SessionLogEntry::TurnEnd {
                through_seq,
                fence_token: 3,
                timestamp: None,
                usage: None,
            },
        )
    }

    fn edit_deleting(seq: u64, from: usize, to: usize) -> (u64, SessionLogEntry) {
        (
            seq,
            SessionLogEntry::EditEntries {
                from,
                to,
                replacements: vec![],
            },
        )
    }

    fn stalled(
        entries: &[(u64, SessionLogEntry)],
        prompt_admitted_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Option<StalledTurn> {
        let prompt_seq = harnx_core::session_reconstruct::pending_prompt_seq(entries);
        stalled_turn(&StallRead {
            entries,
            worker_active: false,
            prompt_seq,
            prompt_admitted_at,
            now,
            lease_acquisition_timeout: ACQUISITION_TIMEOUT,
        })
    }

    fn assert_unclaimed(result: Option<StalledTurn>, prompt_seq: u64) {
        assert_eq!(
            result,
            Some(StalledTurn {
                prompt_seq,
                reason: StallReason::Unclaimed
            })
        );
    }

    fn assert_worker_lost(result: Option<StalledTurn>, prompt_seq: u64) {
        assert_eq!(
            result,
            Some(StalledTurn {
                prompt_seq,
                reason: StallReason::WorkerLost
            })
        );
    }

    #[test]
    fn young_unclaimed_prompt_stays_running() {
        let admitted_at = Some(now());
        let log = [user_prompt(1)];
        // Younger than the acquisition timeout: a worker may still claim it.
        assert_eq!(stalled(&log, admitted_at, now()), None);
    }

    #[test]
    fn unclaimed_prompt_past_acquisition_timeout_is_stalled() {
        let admitted_at = now() - chrono::Duration::from_std(ACQUISITION_TIMEOUT).unwrap();
        let read_at = admitted_at + ACQUISITION_TIMEOUT;
        let log = [user_prompt(1)];
        assert_unclaimed(stalled(&log, Some(admitted_at), read_at), 1);
    }

    #[test]
    fn unclaimed_prompt_one_second_before_the_timeout_stays_running() {
        // The boundary is inclusive: the stall starts at the timeout itself.
        let read_at = now();
        let admitted_at = read_at - ACQUISITION_TIMEOUT + chrono::Duration::seconds(1);
        let log = [user_prompt(1)];
        assert_eq!(stalled(&log, Some(admitted_at), read_at), None);
    }

    #[test]
    fn read_clock_behind_the_admission_clock_stays_running() {
        // Skew or a reordered write: a negative age cannot prove staleness.
        let admitted_at = now() + chrono::Duration::hours(1);
        let log = [user_prompt(1)];
        assert_eq!(stalled(&log, Some(admitted_at), now()), None);
    }

    #[test]
    fn unclaimed_prompt_without_an_admission_time_stays_running() {
        // The row predates admission records and carries no timestamp: age is
        // unprovable, so the reader must not invent a stall.
        let log = [user_prompt(1)];
        assert_eq!(stalled(&log, None, now()), None);
    }

    #[test]
    fn worker_write_within_grace_stays_running() {
        let at = now();
        let log = [user_prompt(1), fenced_write(2, Some(at))];
        assert_eq!(
            stalled(&log, Some(at), at + GRACE - chrono::Duration::seconds(1)),
            None
        );
    }

    #[test]
    fn worker_write_past_grace_is_worker_lost() {
        let at = now();
        let log = [user_prompt(1), fenced_write(2, Some(at))];
        let later = at + GRACE;
        assert_worker_lost(stalled(&log, Some(at), later), 1);
    }

    #[test]
    fn grace_runs_from_the_latest_worker_write() {
        let at = now();
        let log = [
            user_prompt(1),
            fenced_write(2, Some(at)),
            fenced_write(3, Some(at + chrono::Duration::seconds(5))),
        ];
        // Past grace for the first write, within it for the second.
        let later = at + GRACE + chrono::Duration::seconds(1);
        assert_eq!(stalled(&log, Some(at), later), None);
        let later = at + GRACE + chrono::Duration::seconds(5);
        assert_worker_lost(stalled(&log, Some(at), later), 1);
    }

    #[test]
    fn worker_write_without_timestamp_stays_running_even_with_a_stale_admission() {
        // An approval decision is the only worker row that carries no
        // timestamp, and the worker that wrote it may still be re-claiming the
        // lease to resume. Its admission time is stale by minutes, so using it
        // as the reference would declare the worker lost with zero grace —
        // there is no reference instead, and the read stays running.
        let at = now();
        let log = [user_prompt(1), fenced_write(2, None)];
        let stale_admission = at - chrono::Duration::hours(1);
        assert_eq!(stalled(&log, Some(stale_admission), at), None);
        assert_eq!(
            stalled(&log, Some(stale_admission), at + chrono::Duration::hours(1)),
            None
        );
    }

    #[test]
    fn worker_write_with_no_time_reference_stays_running() {
        // An approval decision is the only worker row without a timestamp,
        // and the worker that wrote it may still be re-claiming the lease to
        // resume. With no timestamp and no admission to age the turn from, the
        // read must not invent a verdict.
        let log = [user_prompt(1), fenced_write(2, None)];
        assert_eq!(stalled(&log, None, now()), None);
    }

    #[test]
    fn approval_decision_clears_an_older_worker_write_s_reference() {
        // The worker's last row before its lease vanished is the decision,
        // which records no time. The earlier timestamped write must not age
        // through grace across it, or the read would declare the worker lost
        // the moment the approval resolved while the worker may be resuming.
        let at = now();
        let log = [
            user_prompt(1),
            fenced_write(2, Some(at)),
            approval_decision(3),
        ];
        assert_eq!(stalled(&log, None, at + chrono::Duration::hours(1)), None);
    }

    #[test]
    fn worker_write_after_an_approval_decision_re_establishes_grace() {
        let at = now();
        let log = [
            user_prompt(1),
            approval_decision(2),
            fenced_write(3, Some(at)),
        ];
        // Within grace of the write that follows the decision: still running.
        assert_eq!(
            stalled(&log, None, at + GRACE - chrono::Duration::seconds(1)),
            None
        );
        assert_worker_lost(stalled(&log, None, at + GRACE), 1);
    }

    #[test]
    fn resolved_approval_decision_is_not_worker_loss() {
        // The gate is resolved and the worker is expected to re-claim the lease
        // and resume; neither row carries a timestamp to age the turn from,
        // and the long-stale admission time must not stand in for one.
        let log = [
            user_prompt(1),
            (
                2,
                SessionLogEntry::HitlApprovalRequested {
                    tool_call_id: "derived-call".into(),
                    summary: "approve".into(),
                    fence_token: 7,
                },
            ),
            approval_decision(3),
        ];
        let stale_admission = now() - chrono::Duration::hours(1);
        assert_eq!(stalled(&log, Some(stale_admission), now()), None);
    }

    #[test]
    fn far_future_worker_timestamp_stays_running() {
        // A corrupted or far-future timestamp must not overflow the grace
        // deadline — the derivation compares elapsed time, never adds to it —
        // and reads as young.
        let at = now();
        let log = [
            user_prompt(1),
            fenced_write(2, Some(at + chrono::Duration::days(365 * 400))),
        ];
        assert_eq!(stalled(&log, None, at), None);
    }

    #[test]
    fn lease_present_is_running_even_when_old() {
        let at = now();
        let log = [user_prompt(1), fenced_write(2, Some(at))];
        let later = at + GRACE + chrono::Duration::seconds(60);
        let prompt_seq = harnx_core::session_reconstruct::pending_prompt_seq(&log);
        assert_eq!(
            stalled_turn(&StallRead {
                entries: &log,
                worker_active: true,
                prompt_seq,
                prompt_admitted_at: Some(at),
                now: later,
                lease_acquisition_timeout: ACQUISITION_TIMEOUT,
            }),
            None
        );
    }

    #[test]
    fn terminated_turn_is_never_stalled() {
        let at = Some(now() - chrono::Duration::hours(1));
        let log = [user_prompt(1), turn_end(2, 1)];
        assert_eq!(stalled(&log, at, now()), None);
    }

    #[test]
    fn worker_loss_is_read_from_the_turn_in_progress() {
        // The completed turn's fenced writes must not count as evidence for
        // the pending prompt that follows them.
        let at = now();
        let log = [
            user_prompt(1),
            fenced_write(2, Some(at - chrono::Duration::hours(1))),
            turn_end(3, 1),
            user_prompt(4),
        ];
        assert_eq!(stalled(&log, Some(at), at + GRACE), None);
    }

    #[test]
    fn edit_deleted_error_keeps_the_worker_s_evidence() {
        // The user re-opened the turn by deleting the `Error` that ended it.
        // The worker's fenced write still belongs to the turn, so its age —
        // not the prompt's admission — decides the verdict.
        let at = now();
        let log = [
            user_prompt(1),
            fenced_write(2, Some(at)),
            (
                3,
                SessionLogEntry::Error {
                    message: "provider returned 500".into(),
                    fence_token: 1,
                    timestamp: None,
                },
            ),
            edit_deleting(4, 3, 3),
        ];
        assert_worker_lost(stalled(&log, None, at + GRACE), 1);
    }

    #[test]
    fn rewind_past_the_error_keeps_the_worker_s_evidence() {
        // A rewind re-opens the turn exactly like an edit that deletes the
        // `Error`: the worker's fenced write still belongs to the turn, so
        // its age decides the verdict.
        let at = now();
        let log = [
            user_prompt(1),
            fenced_write(2, Some(at)),
            (
                3,
                SessionLogEntry::Error {
                    message: "provider returned 500".into(),
                    fence_token: 1,
                    timestamp: None,
                },
            ),
            (4, SessionLogEntry::Rewind { after_seq: 2 }),
        ];
        assert_worker_lost(stalled(&log, None, at + GRACE), 1);
    }

    #[test]
    fn edited_worker_row_does_not_leak_into_a_later_turn() {
        // A fenced replacement row inherits its edit's sequence, which can
        // exceed a later prompt's sequence. Evidence is selected positionally
        // after the pending prompt, so a turn-1 row an edit rewrote cannot
        // count against turn-2's prompt as inherited-sequence evidence.
        let at = now();
        let stale = at - chrono::Duration::hours(2);
        let rewritten = format!(
            "type: tool_calls\ntext: rewritten\ncalls: []\ntimestamp: {}\nfence_token: 3\n",
            stale.to_rfc3339()
        );
        let log = [
            user_prompt(1),
            fenced_write(2, Some(stale)),
            turn_end(3, 2),
            (
                4,
                SessionLogEntry::Message {
                    id: Some("prompt-2".into()),
                    role: MessageRole::User,
                    content: MessageContent::Text("unanswered again".into()),
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                10,
                SessionLogEntry::EditEntries {
                    from: 2,
                    to: 2,
                    replacements: vec![rewritten],
                },
            ),
        ];
        // Sequence comparison read the replacement's inherited sequence (10)
        // as turn-2 evidence and reported worker_lost; positionally the
        // rewritten row belongs to turn 1, so the read reports unclaimed.
        assert_unclaimed(stalled(&log, Some(stale), at), 4);
    }
}
