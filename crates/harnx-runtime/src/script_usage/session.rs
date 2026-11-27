use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::message::MessageRole;
use harnx_core::session::{SessionLogEntry, ToolOutput};
use harnx_core::tool::ToolCall;
use serde::Serialize;
use serde_json::Value;

mod payload;

use super::trace::TraceRow;
use crate::config::BASH_TOOL_NAME;

#[derive(Debug, Clone, Copy, Default)]
pub struct TimeWindow {
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
}
impl TimeWindow {
    /// An entry without a timestamp (older sessions) is inside every window.
    pub fn contains(&self, at: Option<DateTime<Utc>>) -> bool {
        let Some(at) = at else { return true };
        self.since.is_none_or(|since| since <= at) && self.until.is_none_or(|until| at < until)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    TimedOut,
    Cancelled,
    Stopped,
    Interrupted,
    LimitHit,
    #[serde(rename = "exit_126")]
    Exit126,
    #[serde(rename = "exit_127")]
    Exit127,
    InnerCallFailed,
    PythonTypeError,
    PythonError,
    NonZeroExit,
    NoResult,
}
impl FailureKind {
    pub const ALL: [FailureKind; 12] = [
        Self::TimedOut,
        Self::Cancelled,
        Self::Stopped,
        Self::Interrupted,
        Self::LimitHit,
        Self::Exit126,
        Self::Exit127,
        Self::InnerCallFailed,
        Self::PythonTypeError,
        Self::PythonError,
        Self::NonZeroExit,
        Self::NoResult,
    ];

    /// The snake_case name the JSON report uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::Stopped => "stopped",
            Self::Interrupted => "interrupted",
            Self::LimitHit => "limit_hit",
            Self::Exit126 => "exit_126",
            Self::Exit127 => "exit_127",
            Self::InnerCallFailed => "inner_call_failed",
            Self::PythonTypeError => "python_type_error",
            Self::PythonError => "python_error",
            Self::NonZeroExit => "non_zero_exit",
            Self::NoResult => "no_result",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailedCall {
    pub name: String,
    pub status: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct ScriptRun {
    pub seq: u64,
    pub timestamp: Option<DateTime<Utc>>,
    pub status: String,
    pub exit_code: Option<i64>,
    pub failure: Option<FailureKind>,
    pub failed_call: Option<FailedCall>,
    pub inner: Vec<TraceRow>,
    pub trace_dropped: u64,
    pub unparsed_trace_lines: u64,
    pub script_bytes: u64,
    pub result_bytes: u64,
    pub output_truncated: bool,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct DirectToolStats {
    pub calls: u64,
    pub errors: u64,
    pub args_bytes: u64,
    pub result_bytes: u64,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct TurnFacts {
    pub finished: bool,
    pub rounds: u64,
    pub direct_calls: u64,
    pub scripts: u64,
    pub usage: Option<CompletionTokenUsage>,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionScriptUsage {
    pub direct: BTreeMap<String, DirectToolStats>,
    pub scripts: Vec<ScriptRun>,
    pub turns: Vec<TurnFacts>,
}
/// One `ToolCalls` entry and the `ToolResults` entry that answered it,
/// borrowed from the log.
struct Round<'a> {
    seq: u64,
    timestamp: Option<DateTime<Utc>>,
    calls: &'a [ToolCall],
    results: Option<&'a [ToolOutput]>,
}

/// What has been seen since the previous turn terminator.
#[derive(Default)]
struct OpenTurn<'a> {
    rounds: Vec<Round<'a>>,
    user_message: bool,
}

/// Reads the log in order, tracking the open turn and the call ids already
/// seen so that rounds compaction logged again are not counted twice.
#[derive(Default)]
struct Analysis<'a> {
    usage: SessionScriptUsage,
    turn: OpenTurn<'a>,
    seen_ids: HashSet<&'a str>,
    skip_results: bool,
}

impl<'a> Analysis<'a> {
    fn tool_calls(&mut self, seq: u64, calls: &'a [ToolCall], timestamp: Option<DateTime<Utc>>) {
        let ids: Option<Vec<&str>> = calls.iter().map(|c| c.id.as_deref()).collect();
        let copy = ids
            .as_ref()
            .is_some_and(|ids| !ids.is_empty() && ids.iter().all(|id| self.seen_ids.contains(id)));
        self.skip_results = copy;
        if copy {
            return;
        }
        self.seen_ids.extend(ids.into_iter().flatten());
        self.turn.rounds.push(Round {
            seq,
            timestamp,
            calls,
            results: None,
        });
    }

    fn tool_results(&mut self, results: &'a [ToolOutput]) {
        if std::mem::take(&mut self.skip_results) {
            return;
        }
        if let Some(round) = self.turn.rounds.last_mut().filter(|r| r.results.is_none()) {
            round.results = Some(results);
        }
    }

    fn end_turn(&mut self, terminator: Option<Terminator>, window: &TimeWindow) {
        self.skip_results = false;
        let turn = std::mem::take(&mut self.turn);
        close_turn(&mut self.usage, turn, window, terminator);
    }
}

/// How a turn ended: `finished`, the entry's timestamp and the turn's usage.
struct Terminator {
    finished: bool,
    timestamp: Option<DateTime<Utc>>,
    usage: Option<CompletionTokenUsage>,
}

impl Terminator {
    fn new(
        finished: bool,
        timestamp: Option<DateTime<Utc>>,
        usage: Option<CompletionTokenUsage>,
    ) -> Self {
        Self {
            finished,
            timestamp,
            usage,
        }
    }
}

/// Turns the raw log of one session into facts about its scripts, its direct
/// tool calls and its turns. Entries it does not know are skipped, so logs
/// written by any version of the program read without error.
pub fn analyze_session(
    entries: &[(u64, SessionLogEntry)],
    window: &TimeWindow,
) -> SessionScriptUsage {
    let mut analysis = Analysis::default();
    for (seq, entry) in entries {
        match entry {
            SessionLogEntry::Message {
                role: MessageRole::User,
                ..
            } => analysis.turn.user_message = true,
            SessionLogEntry::ToolCalls {
                calls, timestamp, ..
            } => analysis.tool_calls(*seq, calls, *timestamp),
            SessionLogEntry::ToolResults { results, .. } => analysis.tool_results(results),
            SessionLogEntry::TurnEnd {
                timestamp, usage, ..
            } => analysis.end_turn(
                Some(Terminator::new(true, *timestamp, usage.clone())),
                window,
            ),
            SessionLogEntry::Cancel { timestamp, .. }
            | SessionLogEntry::Error { timestamp, .. } => {
                analysis.end_turn(Some(Terminator::new(false, *timestamp, None)), window)
            }
            _ => {}
        }
    }
    analysis.end_turn(None, window);
    analysis.usage
}

/// Ends a turn. `None` closes the unfinished tail, which counts only when it
/// holds a round. A turn with no round that ends in `Cancel` or `Error` counts
/// only when a user message started it; otherwise it was a stray stop written
/// while the session was idle.
fn close_turn(
    usage: &mut SessionScriptUsage,
    turn: OpenTurn,
    window: &TimeWindow,
    terminator: Option<Terminator>,
) {
    let rounds = turn.rounds;
    let empty_tail = rounds.is_empty() && terminator.is_none();
    let stray_stop =
        rounds.is_empty() && !turn.user_message && terminator.as_ref().is_some_and(|t| !t.finished);
    if empty_tail || stray_stop {
        return;
    }
    let (finished, end_at, turn_usage) = match terminator {
        Some(t) => (t.finished, t.timestamp, t.usage),
        None => (false, None, None),
    };
    let started = rounds.iter().find_map(|r| r.timestamp).or(end_at);
    if !window.contains(started) {
        return;
    }
    let mut facts = TurnFacts {
        finished,
        rounds: rounds.len() as u64,
        usage: turn_usage,
        ..TurnFacts::default()
    };
    for round in &rounds {
        add_round(usage, round, &mut facts);
    }
    usage.turns.push(facts);
}

fn add_round(usage: &mut SessionScriptUsage, round: &Round<'_>, facts: &mut TurnFacts) {
    for (index, call) in round.calls.iter().enumerate() {
        let out = matching_output(round, index, call);
        if call.name == BASH_TOOL_NAME {
            facts.scripts += 1;
            usage.scripts.push(payload::script_run(round, call, out));
        } else {
            facts.direct_calls += 1;
            let stats = usage.direct.entry(call.name.clone()).or_default();
            add_direct(stats, call, out);
        }
    }
}

/// A result belongs to a call by `id` when both carry one, else by position.
fn matching_output<'a>(round: &Round<'a>, index: usize, call: &ToolCall) -> Option<&'a ToolOutput> {
    let results = round.results?;
    match &call.id {
        Some(id) => results
            .iter()
            .find(|r| r.id.as_ref() == Some(id))
            .or_else(|| results.get(index).filter(|r| r.id.is_none())),
        None => results.get(index),
    }
}

fn is_error_output(output: &Value) -> bool {
    output.get("is_error").and_then(Value::as_bool) == Some(true)
}

fn json_len(value: &Value) -> u64 {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len() as u64)
}

fn add_direct(stats: &mut DirectToolStats, call: &ToolCall, out: Option<&ToolOutput>) {
    stats.calls += 1;
    stats.args_bytes = stats.args_bytes.saturating_add(json_len(&call.arguments));
    if let Some(out) = out {
        stats.result_bytes = stats.result_bytes.saturating_add(json_len(&out.output));
        stats.errors += u64::from(is_error_output(&out.output));
    }
}
