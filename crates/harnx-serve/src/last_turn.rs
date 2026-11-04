//! How the session's most recent turn ended, for `session/get`.
//!
//! A client that stopped following a run, because its stream dropped or it
//! restarted, finds the session `idle` afterwards whatever happened to the
//! turn. The transcript can't tell it either: `history_snapshot` carries
//! messages only, so a turn that failed or hit its time limit partway through
//! looks like one that finished with a short reply. The durable log does
//! record the outcome, in the terminator that ended the turn.

use chrono::{DateTime, Utc};
use harnx_core::session::SessionLogEntry;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct LastTurn {
    /// Log sequence of the entry that ended the turn.
    pub seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(flatten)]
    pub outcome: LastTurnOutcome,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum LastTurnOutcome {
    /// A `TurnEnd`: the worker finished its model, tool and stop-hook loop.
    Completed,
    /// An `Error`: the worker gave up on the turn.
    Error { message: String },
    /// A `Cancel` the worker wrote when the run reached its deadline.
    TimedOut { deadline: DateTime<Utc> },
    /// Any other `Cancel`, from a user, a parent session or a frontend. The
    /// same entry makes `state` read `interrupted`.
    Interrupted {
        #[serde(skip_serializing_if = "Option::is_none")]
        requested_by: Option<String>,
    },
}

/// The last `TurnEnd`, `Error` or `Cancel` in the log, which is the terminator
/// `session/get`'s `interrupted` state reads too. `None` until a turn ends.
pub(crate) fn last_turn(entries: &[(u64, SessionLogEntry)]) -> Option<LastTurn> {
    entries
        .iter()
        .rev()
        .find_map(|(seq, entry)| last_turn_from_entry(*seq, entry))
}

fn last_turn_from_entry(seq: u64, entry: &SessionLogEntry) -> Option<LastTurn> {
    let (ended_at, outcome) = match entry {
        SessionLogEntry::TurnEnd { timestamp, .. } => (*timestamp, LastTurnOutcome::Completed),
        SessionLogEntry::Error {
            message, timestamp, ..
        } => (
            *timestamp,
            LastTurnOutcome::Error {
                message: message.clone(),
            },
        ),
        SessionLogEntry::Cancel {
            requested_by,
            timestamp,
            ..
        } => (*timestamp, cancel_outcome(requested_by.as_deref())),
        _ => return None,
    };
    Some(LastTurn {
        seq,
        ended_at,
        outcome,
    })
}

/// A worker that stops a run at its deadline records the deadline in the
/// `Cancel`'s `requested_by`, as `harnx:timeout {...}`.
fn cancel_outcome(requested_by: Option<&str>) -> LastTurnOutcome {
    match requested_by.and_then(harnx_runtime::parse_timeout_terminal) {
        Some(timeout) => LastTurnOutcome::TimedOut {
            deadline: timeout.deadline,
        },
        None => LastTurnOutcome::Interrupted {
            requested_by: requested_by.map(str::to_owned),
        },
    }
}

#[cfg(test)]
mod tests;
