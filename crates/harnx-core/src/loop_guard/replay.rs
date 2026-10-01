//! Replays a stored session through the live guards, for
//! `harnx dump session --check-loop-detection`: its tool calls through the
//! tool-call guard and its saved replies through the repeated-output rule.

use super::output_repeat::{detect_in_text, OutputChannel, RepeatedTail};
use super::tool_repeat::{strip_note, ToolRepeatGuard, ToolRepeatVerdict};
use crate::message::MessageRole;
use crate::session::{SessionLogEntry, ToolOutput};
use crate::tool::ToolCall;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt::Write as _;

/// How the live guard would have reacted to a call.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopCheckEventKind {
    /// The call ran and its result repeated, so the live run would have
    /// appended a note to the result.
    Note,
    /// The live run would not have executed the call.
    Refusal,
    /// The live run would have ended the turn at this call.
    Stop,
}

/// One call the live guard would have reacted to.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LoopCheckEvent {
    /// Log sequence of the entry that requested the call.
    pub seq: u64,
    /// When the call was requested; `None` when the log carries no time for it.
    pub timestamp: Option<DateTime<Utc>>,
    pub kind: LoopCheckEventKind,
    pub tool: String,
    pub arguments: Value,
    /// Identical calls with identical results the guard counted in its window
    /// when it reacted.
    pub count: usize,
}

/// A saved reply the live output guard would have stopped.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OutputCheckEvent {
    /// Log sequence of the entry holding the reply.
    pub seq: u64,
    pub timestamp: Option<DateTime<Utc>>,
    pub channel: OutputChannel,
    /// The repeated unit, cut to 120 characters.
    pub unit: String,
    pub unit_len: usize,
    /// Characters into the channel when the repeat was found.
    pub at_char: usize,
}

impl OutputCheckEvent {
    fn new(
        seq: u64,
        timestamp: Option<DateTime<Utc>>,
        channel: OutputChannel,
        tail: RepeatedTail,
    ) -> Self {
        Self {
            seq,
            timestamp,
            channel,
            unit_len: tail.unit.chars().count(),
            unit: tail.unit.chars().take(120).collect(),
            at_char: tail.at_char,
        }
    }
}

/// One reply's two channels.
struct ReplyText<'a> {
    answer: &'a str,
    thinking: Option<&'a str>,
}

impl<'a> ReplyText<'a> {
    /// The reply of a response that asked for tool calls. A thought saved
    /// apart is the thinking as it is; without one, the text can still hold
    /// the reasoning inline, as a saved message does.
    fn of_tool_calls(text: &'a str, thought: Option<&'a str>) -> Self {
        match thought {
            Some(_) => Self {
                answer: text,
                thinking: thought,
            },
            None => Self::from_saved(text),
        }
    }

    /// A saved reply keeps its reasoning inline as `<think>\n…\n</think>\n`
    /// before the answer; split it back into the two channels.
    fn from_saved(text: &'a str) -> Self {
        let split = text
            .strip_prefix("<think>\n")
            .and_then(|rest| rest.split_once("\n</think>\n"));
        match split {
            Some((thinking, answer)) => Self {
                answer,
                thinking: Some(thinking),
            },
            None => Self {
                answer: text,
                thinking: None,
            },
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct LoopCheckReport {
    /// Every call in the log, including those after a stop that the replay
    /// did not decide.
    pub tool_calls: usize,
    pub events: Vec<LoopCheckEvent>,
    /// Saved replies the output guard would have stopped. A stream the live
    /// guard stopped was never saved, so it cannot appear here.
    pub output_events: Vec<OutputCheckEvent>,
}

impl LoopCheckReport {
    pub fn count(&self, kind: LoopCheckEventKind) -> usize {
        self.events
            .iter()
            .filter(|event| event.kind == kind)
            .count()
    }

    pub fn render_text(&self) -> String {
        let mut out = String::new();
        self.render_tool_events(&mut out);
        self.render_output_events(&mut out);
        self.render_summaries(&mut out);
        self.render_limits(&mut out);
        out
    }

    fn render_tool_events(&self, out: &mut String) {
        for event in &self.events {
            let time = format_time(event.timestamp);
            let what = kind_label(event.kind);
            let arguments: String = event.arguments.to_string().chars().take(120).collect();
            let _ = writeln!(
                out,
                "seq {} {time} {what}: {} {arguments} ({} identical)",
                event.seq, event.tool, event.count
            );
        }
    }

    fn render_output_events(&self, out: &mut String) {
        for event in &self.output_events {
            let time = format_time(event.timestamp);
            let channel = channel_label(event.channel);
            let _ = writeln!(
                out,
                "seq {} {time} repeated {channel}: {:?} (unit of {} chars, caught at char {})",
                event.seq, event.unit, event.unit_len, event.at_char
            );
        }
    }

    fn render_summaries(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "{} tool calls: {} notes, {} refusals, {} stops.",
            self.tool_calls,
            self.count(LoopCheckEventKind::Note),
            self.count(LoopCheckEventKind::Refusal),
            self.count(LoopCheckEventKind::Stop),
        );
        let answers = self
            .output_events
            .iter()
            .filter(|event| event.channel == OutputChannel::Answer)
            .count();
        let _ = writeln!(
            out,
            "{} repeated outputs ({answers} answer, {} thinking).",
            self.output_events.len(),
            self.output_events.len() - answers
        );
    }

    fn render_limits(&self, out: &mut String) {
        out.push_str(
            "Replay limits: after a refusal the real session may have run the call and gone on, \
             so later events may differ from a live run; after a stop the replay skips to the \
             next turn; a response stopped for repeating itself was never saved, so only saved \
             replies are checked for repeated output.\n",
        );
    }
}

/// Format a timestamp for text output.
fn format_time(timestamp: Option<DateTime<Utc>>) -> String {
    timestamp
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "-".to_string())
}

/// Label for a loop check event kind.
fn kind_label(kind: LoopCheckEventKind) -> &'static str {
    match kind {
        LoopCheckEventKind::Note => "note",
        LoopCheckEventKind::Refusal => "refused",
        LoopCheckEventKind::Stop => "turn stopped",
    }
}

/// Label for an output channel.
fn channel_label(channel: OutputChannel) -> &'static str {
    match channel {
        OutputChannel::Answer => "answer",
        OutputChannel::Thinking => "thinking",
    }
}

/// Replay `entries` (as returned by `apply_log_mutations_nats`). The guard
/// resets where the live one does: each user message, each compaction, and
/// each turn end.
pub fn check_session(entries: &[(u64, SessionLogEntry)]) -> LoopCheckReport {
    let mut replay = Replay::default();
    for (seq, entry) in entries {
        replay.apply(*seq, entry);
    }
    replay.report
}

/// A stored result as the tool returned it, without the note a guarded
/// session added. `None` for a call the live guard refused or stopped: it
/// never ran, so its stored result is harnx's own. The replay can still have
/// let such a call run, because its clock and the live guard's differ
/// slightly at the edge of the window.
fn tool_output(stored: &Value) -> Option<Value> {
    stored
        .get("loop_guard")
        .is_none()
        .then(|| strip_note(stored))
}

/// A call as the log recorded it: where it sits and when it was requested.
struct LoggedCall {
    call: ToolCall,
    at: DateTime<Utc>,
    seq: u64,
}

#[derive(Default)]
struct Replay {
    guard: ToolRepeatGuard,
    // Calls the guard let through, waiting for their result by call id.
    pending: HashMap<String, LoggedCall>,
    stopped: bool,
    clock: Option<DateTime<Utc>>,
    report: LoopCheckReport,
}

impl Replay {
    fn apply(&mut self, seq: u64, entry: &SessionLogEntry) {
        match entry {
            SessionLogEntry::Message {
                role: MessageRole::User,
                ..
            }
            | SessionLogEntry::TurnEnd { .. }
            | SessionLogEntry::Error { .. }
            | SessionLogEntry::Cancel { .. }
            | SessionLogEntry::Compress { .. }
            | SessionLogEntry::CompactRequest { .. }
            | SessionLogEntry::CompactResult { .. } => self.restart(),
            SessionLogEntry::Message {
                role: MessageRole::Assistant,
                content,
                timestamp,
                ..
            } => self.scan_reply(seq, *timestamp, ReplyText::from_saved(&content.to_text())),
            SessionLogEntry::ToolCalls {
                text,
                thought,
                calls,
                timestamp,
                ..
            } => {
                let reply = ReplyText::of_tool_calls(text, thought.as_deref());
                self.scan_reply(seq, *timestamp, reply);
                self.calls(seq, calls, *timestamp)
            }
            SessionLogEntry::ToolResults { results, .. } => self.results(results),
            _ => {}
        }
    }

    fn restart(&mut self) {
        self.guard.reset();
        self.pending.clear();
        self.stopped = false;
    }

    // An entry without a timestamp takes the previous one, so a gap in the log
    // never makes calls look further apart than they were.
    fn now(&mut self, timestamp: Option<DateTime<Utc>>) -> DateTime<Utc> {
        let now = timestamp.or(self.clock).unwrap_or(DateTime::<Utc>::MIN_UTC);
        self.clock = Some(now);
        now
    }

    fn calls(&mut self, seq: u64, calls: &[ToolCall], timestamp: Option<DateTime<Utc>>) {
        let at = self.now(timestamp);
        self.report.tool_calls += calls.len();
        // One entry holds the calls of one model response.
        self.guard.begin_batch();
        for call in calls {
            self.decide(seq, at, call);
        }
    }

    // After a stop the turn is over, so its remaining calls are counted but
    // never decided.
    fn decide(&mut self, seq: u64, at: DateTime<Utc>, call: &ToolCall) {
        if self.stopped {
            return;
        }
        let verdict = self.guard.decide(call, at);
        let logged = LoggedCall {
            call: call.clone(),
            at,
            seq,
        };
        match verdict {
            ToolRepeatVerdict::Run => self.await_result(logged),
            ToolRepeatVerdict::Refuse(refusal) => {
                self.push(logged, LoopCheckEventKind::Refusal, refusal.count)
            }
            ToolRepeatVerdict::Stop(terminal) => {
                self.push(
                    logged,
                    LoopCheckEventKind::Stop,
                    terminal.count.unwrap_or_default(),
                );
                // The live guard runs no call of a stopped round, not even
                // those decided before the stop.
                self.pending.clear();
                self.stopped = true;
            }
        }
    }

    // A call without an id cannot be matched to its result, so the guard
    // never records it.
    fn await_result(&mut self, logged: LoggedCall) {
        if let Some(id) = logged.call.id.clone() {
            self.pending.insert(id, logged);
        }
    }

    fn results(&mut self, results: &[ToolOutput]) {
        for result in results {
            let Some(logged) = result.id.as_ref().and_then(|id| self.pending.remove(id)) else {
                continue;
            };
            let Some(output) = tool_output(&result.output) else {
                continue;
            };
            if let Some(note) = self.guard.record(&logged.call, &output, logged.at) {
                self.push(logged, LoopCheckEventKind::Note, note.count);
            }
        }
    }

    fn push(&mut self, logged: LoggedCall, kind: LoopCheckEventKind, count: usize) {
        self.report.events.push(LoopCheckEvent {
            seq: logged.seq,
            timestamp: (logged.at != DateTime::<Utc>::MIN_UTC).then_some(logged.at),
            kind,
            tool: logged.call.name,
            arguments: logged.call.arguments,
            count,
        });
    }

    /// Check a saved reply's thinking and answer the way the live output
    /// guard checks a stream. The rule keeps no state, so nothing resets.
    fn scan_reply(&mut self, seq: u64, timestamp: Option<DateTime<Utc>>, reply: ReplyText<'_>) {
        let channels = [
            (OutputChannel::Thinking, reply.thinking.unwrap_or_default()),
            (OutputChannel::Answer, reply.answer),
        ];
        for (channel, text) in channels {
            if let Some(tail) = detect_in_text(text) {
                self.report
                    .output_events
                    .push(OutputCheckEvent::new(seq, timestamp, channel, tail));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loop_guard::{
        append_note, OutputChannel, RepeatNote, RepetitionStop, RepetitionTerminal,
    };
    use crate::message::{MessageContent, MessageRole};
    use crate::session::{CompactOutcome, SessionLogEntry, ToolOutput};
    use serde_json::json;

    fn at(secs: i64) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0)
    }

    fn user(text: &str) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text(text.to_string()),
            timestamp: at(0),
            fence_token: None,
        }
    }

    fn calls(id: &str, timestamp: Option<DateTime<Utc>>) -> SessionLogEntry {
        SessionLogEntry::ToolCalls {
            text: String::new(),
            thought: None,
            calls: vec![ToolCall::new(
                "fs_read".into(),
                json!({"path": "a.rs", "offset": 70}),
                Some(id.to_string()),
                None,
            )],
            timestamp,
            fence_token: None,
        }
    }

    fn results(id: &str) -> SessionLogEntry {
        SessionLogEntry::ToolResults {
            results: vec![ToolOutput {
                id: Some(id.to_string()),
                name: "fs_read".into(),
                output: json!("same"),
                markdown: None,
                content: vec![],
                switch_agent: None,
            }],
            timestamp: None,
        }
    }

    /// A user prompt followed by `n` identical read rounds two seconds apart.
    fn looping_session(n: usize) -> Vec<(u64, SessionLogEntry)> {
        let mut entries = vec![user("fix it")];
        for i in 0..n {
            let id = format!("c{i}");
            entries.push(calls(&id, at(2 * i as i64)));
            entries.push(results(&id));
        }
        entries
            .into_iter()
            .enumerate()
            .map(|(seq, e)| (seq as u64, e))
            .collect()
    }

    #[test]
    fn a_loop_reports_notes_one_refusal_and_a_stop() {
        let report = check_session(&looping_session(20));
        assert_eq!(report.tool_calls, 20);
        assert_eq!(report.count(LoopCheckEventKind::Note), 3);
        assert_eq!(report.count(LoopCheckEventKind::Refusal), 1);
        assert_eq!(
            report.count(LoopCheckEventKind::Stop),
            1,
            "the replay skips to the next turn"
        );
        let stop = report
            .events
            .iter()
            .find(|e| e.kind == LoopCheckEventKind::Stop)
            .unwrap();
        assert_eq!(stop.tool, "fs_read");
        assert_eq!(stop.count, 4);
    }

    #[test]
    fn a_user_message_starts_the_count_over() {
        let mut entries = looping_session(4);
        entries.push((100, user("try again")));
        for i in 0..4 {
            let id = format!("d{i}");
            entries.push((101 + 2 * i, calls(&id, at(100 + i as i64))));
            entries.push((102 + 2 * i, results(&id)));
        }
        let report = check_session(&entries);
        assert_eq!(report.count(LoopCheckEventKind::Refusal), 0);
        assert_eq!(report.count(LoopCheckEventKind::Stop), 0);
    }

    #[test]
    fn entries_without_timestamps_take_the_previous_time() {
        let mut entries = vec![(0, user("go"))];
        for i in 0..6u64 {
            let id = format!("n{i}");
            let timestamp = if i == 0 { at(0) } else { None };
            entries.push((1 + 2 * i, calls(&id, timestamp)));
            entries.push((2 + 2 * i, results(&id)));
        }
        let report = check_session(&entries);
        assert_eq!(report.count(LoopCheckEventKind::Refusal), 1);
    }

    #[test]
    fn text_report_ends_with_a_summary_and_the_replay_limits() {
        let text = check_session(&looping_session(6)).render_text();
        assert!(text.contains("refused: fs_read"));
        assert!(text.contains("6 tool calls: 3 notes, 1 refusals, 1 stops."));
        // A session recorded with the guard on did not run its refused calls.
        assert!(text.ends_with(
            "Replay limits: after a refusal the real session may have run the call and gone on, \
             so later events may differ from a live run; after a stop the replay skips to the \
             next turn; a response stopped for repeating itself was never saved, so only saved \
             replies are checked for repeated output.\n"
        ));
    }

    fn push_entry(entries: &mut Vec<(u64, SessionLogEntry)>, entry: SessionLogEntry) {
        let seq = entries.last().map_or(0, |(seq, _)| seq + 1);
        entries.push((seq, entry));
    }

    /// Appends `n` identical read rounds two seconds apart, the first at
    /// `start`, or with no timestamps at all when `start` is `None`.
    fn push_rounds(
        entries: &mut Vec<(u64, SessionLogEntry)>,
        prefix: &str,
        n: usize,
        start: Option<i64>,
    ) {
        for i in 0..n {
            let id = format!("{prefix}{i}");
            let timestamp = start.and_then(|start| at(start + 2 * i as i64));
            push_entry(entries, calls(&id, timestamp));
            push_entry(entries, results(&id));
        }
    }

    /// Every entry after which a live turn starts its tool rounds afresh.
    fn round_boundaries() -> Vec<SessionLogEntry> {
        vec![
            user("go on"),
            SessionLogEntry::TurnEnd {
                through_seq: 0,
                fence_token: 0,
                timestamp: None,
                usage: None,
            },
            SessionLogEntry::Error {
                message: "worker failed".into(),
                fence_token: 0,
                timestamp: None,
            },
            SessionLogEntry::cancel_request("cancel-1".into(), "tester".into()),
            SessionLogEntry::Compress {
                prompt: "summarize".into(),
            },
            SessionLogEntry::compact_request("compact-1", None),
            SessionLogEntry::compact_result("compact-1", CompactOutcome::Compacted),
        ]
    }

    #[test]
    fn a_stop_ends_only_the_turn_it_happened_in() {
        for boundary in round_boundaries() {
            let mut entries = looping_session(20);
            push_entry(&mut entries, boundary.clone());
            push_rounds(&mut entries, "next", 20, Some(200));
            let report = check_session(&entries);
            let counts = (
                report.tool_calls,
                report.count(LoopCheckEventKind::Note),
                report.count(LoopCheckEventKind::Refusal),
                report.count(LoopCheckEventKind::Stop),
            );
            assert_eq!(counts, (40, 6, 2, 2), "after {boundary:?}");
        }
    }

    fn read_of(id: &str, path: &str) -> ToolCall {
        ToolCall::new(
            "fs_read".into(),
            json!({"path": path}),
            Some(id.to_string()),
            None,
        )
    }

    /// One model response requesting `calls` at `timestamp`.
    fn response(calls: Vec<ToolCall>, timestamp: Option<DateTime<Utc>>) -> SessionLogEntry {
        SessionLogEntry::ToolCalls {
            text: String::new(),
            thought: None,
            calls,
            timestamp,
            fence_token: None,
        }
    }

    /// The stored results of `fs_read` calls, by call id.
    fn outputs(results: Vec<(&str, Value)>) -> SessionLogEntry {
        SessionLogEntry::ToolResults {
            results: results
                .into_iter()
                .map(|(id, output)| ToolOutput {
                    id: Some(id.to_string()),
                    name: "fs_read".into(),
                    output,
                    markdown: None,
                    content: vec![],
                    switch_agent: None,
                })
                .collect(),
            timestamp: None,
        }
    }

    /// Appends one response asking for `calls` at `secs`, and their stored
    /// results.
    fn push_response(
        entries: &mut Vec<(u64, SessionLogEntry)>,
        calls: Vec<ToolCall>,
        secs: i64,
        stored: Vec<(&str, Value)>,
    ) {
        push_entry(entries, response(calls, at(secs)));
        push_entry(entries, outputs(stored));
    }

    /// Appends one response reading a.rs as call `id` at `secs`, and the
    /// call's stored `output`.
    fn push_read(entries: &mut Vec<(u64, SessionLogEntry)>, id: &str, secs: i64, output: Value) {
        push_response(entries, vec![read_of(id, "a.rs")], secs, vec![(id, output)]);
    }

    fn kinds(report: &LoopCheckReport) -> Vec<LoopCheckEventKind> {
        report.events.iter().map(|event| event.kind).collect()
    }

    /// The result the live guard stores for a call it refused, as
    /// `screen_round` in harnx-runtime's `tool_loop_guard.rs` builds it.
    fn refused_output() -> Value {
        let refusal = crate::loop_guard::Refusal {
            tool: "fs_read".into(),
            count: 4,
            retry_after: at(600).unwrap(),
        };
        json!({"is_error": true, "error": refusal.message(), "loop_guard": "refused"})
    }

    /// The result the live guard stores for each call of a stopped round, as
    /// `stopped_output` in harnx-runtime's `tool_loop_guard.rs` builds it.
    fn stopped_output() -> Value {
        let terminal = RepetitionTerminal::tool_calls("fs_read", 4);
        json!({
            "is_error": true,
            "error": format!("harnx ended the turn: {}.", terminal.reason()),
            "loop_guard": "stopped",
        })
    }

    /// The log of a turn the live guard stopped: notes on the 2nd to 4th
    /// results, then a refused call, a stopped call and the turn's error.
    fn guarded_session() -> Vec<(u64, SessionLogEntry)> {
        let body = json!({"content": [{"type": "text", "text": "70: fn x() {}"}]});
        let mut entries = vec![(0, user("fix it"))];
        for i in 0..4 {
            let mut output = body.clone();
            if i > 0 {
                append_note(&mut output, &RepeatNote { count: i + 1 }.text());
            }
            push_read(&mut entries, &format!("c{i}"), 2 * i as i64, output);
        }
        push_read(&mut entries, "c4", 8, refused_output());
        push_read(&mut entries, "c5", 10, stopped_output());
        let stop = RepetitionStop(RepetitionTerminal::tool_calls("fs_read", 4));
        push_entry(
            &mut entries,
            SessionLogEntry::Error {
                message: stop.to_string(),
                fence_token: 0,
                timestamp: None,
            },
        );
        entries
    }

    #[test]
    fn a_session_recorded_with_the_guard_on_reports_what_the_guard_did() {
        let report = check_session(&guarded_session());
        let events: Vec<_> = report
            .events
            .iter()
            .map(|event| (event.kind, event.count))
            .collect();
        use LoopCheckEventKind::{Note, Refusal, Stop};
        assert_eq!(
            events,
            [(Note, 2), (Note, 3), (Note, 4), (Refusal, 4), (Stop, 4)]
        );
    }

    #[test]
    fn results_of_refused_or_stopped_calls_are_never_counted() {
        // At a window edge the replay can run a call the live guard refused.
        // Its stored result is harnx's refusal, not the tool's output, so
        // counting it would report notes the live run never gave.
        let mut entries = vec![(0, user("go"))];
        let stored = [
            refused_output(),
            refused_output(),
            stopped_output(),
            stopped_output(),
        ];
        for (i, output) in stored.into_iter().enumerate() {
            push_read(&mut entries, &format!("r{i}"), 2 * i as i64, output);
        }
        assert_eq!(check_session(&entries).events, []);
    }

    #[test]
    fn a_stop_drops_the_calls_decided_before_it_in_its_response() {
        use LoopCheckEventKind::{Note, Refusal, Stop};
        let mut entries = vec![(0, user("go"))];
        let x0 = vec![read_of("x0", "x.rs")];
        push_response(&mut entries, x0, 0, vec![("x0", json!("x"))]);
        for i in 0..4 {
            push_read(&mut entries, &format!("a{i}"), 1 + i, json!("a"));
        }
        // Two refusals of a.rs, each followed by a call whose result changes.
        for i in 0..2 {
            push_read(&mut entries, &format!("a{}", 4 + i), 10 + 2 * i, json!("a"));
            let other = format!("t{i}");
            let calls = vec![read_of(&other, "t.rs")];
            push_response(&mut entries, calls, 11 + 2 * i, vec![(&other, json!(i))]);
        }
        // x.rs is decided before the third refusal of a.rs stops the turn,
        // but the live guard runs no call of a stopped round.
        let both = vec![read_of("x1", "x.rs"), read_of("a6", "a.rs")];
        let stored = vec![("x1", json!("x")), ("a6", json!("a"))];
        push_response(&mut entries, both, 20, stored);
        assert_eq!(
            kinds(&check_session(&entries)),
            [Note, Note, Note, Refusal, Refusal, Stop]
        );
    }

    #[test]
    fn refusals_within_one_response_are_not_in_a_row() {
        use LoopCheckEventKind::{Note, Refusal, Stop};
        let mut entries = vec![(0, user("go"))];
        for i in 0..4 {
            let (a, b) = (format!("a{i}"), format!("b{i}"));
            let calls = vec![read_of(&a, "a.rs"), read_of(&b, "b.rs")];
            push_entry(&mut entries, response(calls, at(2 * i)));
            push_entry(
                &mut entries,
                outputs(vec![(&a, json!("a")), (&b, json!("b"))]),
            );
        }
        // Both again in one response, before the model saw a refusal, then
        // one of them in the next response.
        let both = vec![read_of("a4", "a.rs"), read_of("b4", "b.rs")];
        push_entry(&mut entries, response(both, at(8)));
        push_entry(&mut entries, response(vec![read_of("b5", "b.rs")], at(10)));
        assert_eq!(
            kinds(&check_session(&entries)),
            [Note, Note, Note, Note, Note, Note, Refusal, Refusal, Stop]
        );
    }

    #[test]
    fn a_call_that_ran_after_a_refusal_in_its_response_does_not_forgive_it() {
        use LoopCheckEventKind::{Note, Refusal, Stop};
        let mut entries = vec![(0, user("go"))];
        for i in 0..4 {
            push_read(&mut entries, &format!("a{i}"), 2 * i, json!("a"));
        }
        // The refused call is followed by one that ran in the same response.
        let calls = vec![read_of("a4", "a.rs"), read_of("t0", "t.rs")];
        let stored = vec![("a4", refused_output()), ("t0", json!("t"))];
        push_response(&mut entries, calls, 8, stored);
        push_read(&mut entries, "a5", 10, json!("a"));
        assert_eq!(
            kinds(&check_session(&entries)),
            [Note, Note, Note, Refusal, Stop]
        );
    }

    #[test]
    fn json_report_uses_snake_case_kinds_and_rfc3339_times() {
        let json = serde_json::to_value(check_session(&looping_session(6))).unwrap();
        let kinds: Vec<&str> = json["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["kind"].as_str().unwrap())
            .collect();
        assert_eq!(json["tool_calls"], 6);
        assert_eq!(kinds, ["note", "note", "note", "refusal", "stop"]);
        assert_eq!(
            json["events"][3],
            json!({
                "seq": 9,
                "timestamp": "2027-01-15T08:00:08Z",
                "kind": "refusal",
                "tool": "fs_read",
                "arguments": {"path": "a.rs", "offset": 70},
                "count": 4,
            })
        );
    }

    #[test]
    fn text_report_lists_each_event_with_its_utc_time_and_count() {
        let text = check_session(&looping_session(6)).render_text();
        let args = json!({"path": "a.rs", "offset": 70});
        assert_eq!(
            text.lines().take(5).collect::<Vec<_>>(),
            [
                format!("seq 3 2027-01-15 08:00:02 note: fs_read {args} (2 identical)"),
                format!("seq 5 2027-01-15 08:00:04 note: fs_read {args} (3 identical)"),
                format!("seq 7 2027-01-15 08:00:06 note: fs_read {args} (4 identical)"),
                format!("seq 9 2027-01-15 08:00:08 refused: fs_read {args} (4 identical)"),
                format!("seq 11 2027-01-15 08:00:10 turn stopped: fs_read {args} (4 identical)"),
            ]
        );
    }

    #[test]
    fn a_timed_call_after_untimed_ones_still_counts_them() {
        let mut entries = vec![(0, user("go"))];
        push_rounds(&mut entries, "a", 1, Some(0));
        push_rounds(&mut entries, "b", 3, None);
        push_rounds(&mut entries, "c", 1, Some(10));
        let report = check_session(&entries);
        assert_eq!(report.count(LoopCheckEventKind::Refusal), 1);
    }

    #[test]
    fn a_log_without_timestamps_reports_unknown_times() {
        let mut entries = vec![(0, user("go"))];
        push_rounds(&mut entries, "u", 5, None);
        let report = check_session(&entries);
        assert_eq!(report.count(LoopCheckEventKind::Refusal), 1);
        assert!(report.events.iter().all(|event| event.timestamp.is_none()));
        assert!(report.render_text().contains("seq 9 - refused: fs_read"));
    }

    #[test]
    fn text_report_shortens_long_arguments() {
        let report = LoopCheckReport {
            tool_calls: 1,
            events: vec![LoopCheckEvent {
                seq: 7,
                timestamp: None,
                kind: LoopCheckEventKind::Note,
                tool: "fs_read".into(),
                arguments: json!({"path": "x".repeat(500)}),
                count: 2,
            }],
            ..Default::default()
        };
        let shown: String = json!({"path": "x".repeat(500)})
            .to_string()
            .chars()
            .take(120)
            .collect();
        assert_eq!(
            report.render_text().lines().next(),
            Some(format!("seq 7 - note: fs_read {shown} (2 identical)").as_str())
        );
    }

    fn assistant(secs: i64, text: impl Into<String>) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text(text.into()),
            timestamp: at(secs),
            fence_token: None,
        }
    }

    #[test]
    fn a_repeating_answer_or_thinking_is_reported() {
        let entries: Vec<(u64, SessionLogEntry)> = vec![
            (0, user("go")),
            (
                1,
                SessionLogEntry::ToolCalls {
                    text: String::new(),
                    thought: Some("Let me check. ".repeat(300)),
                    calls: vec![],
                    timestamp: at(1),
                    fence_token: None,
                },
            ),
            // A saved reply keeps its reasoning inline, before the answer.
            (
                2,
                assistant(
                    2,
                    format!(
                        "<think>\nPlanning.\n</think>\nStart. {}",
                        "again ".repeat(600)
                    ),
                ),
            ),
            (3, assistant(3, "A normal final answer.")),
            (
                4,
                assistant(
                    4,
                    format!("<think>\n{}\n</think>\nDone.", "hmm ".repeat(700)),
                ),
            ),
        ];
        let report = check_session(&entries);
        let found: Vec<(u64, OutputChannel)> = report
            .output_events
            .iter()
            .map(|event| (event.seq, event.channel))
            .collect();
        assert_eq!(
            found,
            [
                (1, OutputChannel::Thinking),
                (2, OutputChannel::Answer),
                (4, OutputChannel::Thinking),
            ]
        );
        assert_eq!(report.output_events[1].unit_len, 6);
        assert_eq!(report.output_events[1].at_char, 2_048);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["output_events"][1]["channel"], "answer");
        let text = report.render_text();
        assert!(text.contains("seq 2 2027-01-15 08:00:02 repeated answer: "));
        assert!(text.contains("3 repeated outputs (1 answer, 2 thinking)."));
    }

    #[test]
    fn a_tool_call_reply_is_split_the_way_a_message_is() {
        // Saved with no thought of its own, the reply keeps its reasoning
        // inline, so the loop sits in the middle of the text, not at its end.
        let inline = SessionLogEntry::ToolCalls {
            text: format!("<think>\n{}\n</think>\nanswer", "hmm ".repeat(700)),
            thought: None,
            calls: vec![],
            timestamp: at(1),
            fence_token: None,
        };
        // A thought saved apart is the thinking as it is, and the text is the
        // answer.
        let apart = SessionLogEntry::ToolCalls {
            text: "again ".repeat(600),
            thought: Some("Planning.".into()),
            calls: vec![],
            timestamp: at(2),
            fence_token: None,
        };
        let report = check_session(&[(0, user("go")), (1, inline), (2, apart)]);
        let found: Vec<(u64, OutputChannel)> = report
            .output_events
            .iter()
            .map(|event| (event.seq, event.channel))
            .collect();
        assert_eq!(
            found,
            [(1, OutputChannel::Thinking), (2, OutputChannel::Answer)]
        );
    }

    #[test]
    fn ordinary_replies_report_no_output_events() {
        let varied: String = (0..300).map(|i| format!("line {i} is new\n")).collect();
        let entries = vec![
            (0, user("go")),
            (1, assistant(1, "fine")),
            (2, assistant(2, &varied)),
        ];
        let report = check_session(&entries);
        assert!(report.output_events.is_empty());
        assert!(report
            .render_text()
            .contains("0 repeated outputs (0 answer, 0 thinking)."));
    }
}
