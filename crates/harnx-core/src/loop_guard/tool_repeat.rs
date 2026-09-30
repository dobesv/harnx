//! Counts identical tool calls with identical results and decides when to
//! warn, refuse, or end the turn.

use super::digest::{fingerprint, Fingerprint};
use super::RepetitionTerminal;
use crate::tool::ToolCall;
use chrono::{DateTime, DurationRound, Local, TimeDelta, Utc};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

/// Identical calls with identical results allowed per window before harnx
/// refuses the next one.
pub const TOOL_REPEAT_LIMIT: usize = 4;

/// How far back identical calls count. Long enough to catch a slow model that
/// spins once every couple of minutes.
pub const TOOL_REPEAT_WINDOW: Duration = Duration::from_secs(600);

/// A call's third refusal in the window ends the turn even when other calls
/// ran in between: one premature retry is forgiven, a second is not. Without
/// this, a loop that alternates the refused call with a call whose result
/// changes would never stop.
const REFUSALS_BEFORE_STOP: usize = 3;

fn window() -> TimeDelta {
    TimeDelta::from_std(TOOL_REPEAT_WINDOW).expect("the window fits in a TimeDelta")
}

fn window_minutes() -> u64 {
    TOOL_REPEAT_WINDOW.as_secs() / 60
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CallKey {
    name: String,
    arguments: Fingerprint,
}

impl CallKey {
    fn of(call: &ToolCall) -> Self {
        Self {
            name: call.name.clone(),
            arguments: fingerprint(&call.arguments),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Execution {
    at: DateTime<Utc>,
    result: Fingerprint,
}

/// A refused call and when it may run again.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Refusal {
    pub tool: String,
    pub count: usize,
    pub retry_after: DateTime<Utc>,
}

impl Refusal {
    /// The error text the model receives in place of the refused call's result.
    pub fn message(&self) -> String {
        // The call runs again only once `retry_after` has passed, so a time
        // floored to the second could name a moment when it is still refused.
        let retry_after = self
            .retry_after
            .duration_round_up(TimeDelta::seconds(1))
            .unwrap_or(self.retry_after)
            .with_timezone(&Local)
            .format("%Y-%m-%dT%H:%M:%S%:z");
        format!(
            "harnx did not run this call. It matches your last {count} calls to `{tool}` in the \
             past {minutes} minutes, and each returned the same result. It can run again after \
             {retry_after}. Use the result you have, wait, or take a different approach. If your \
             next call is refused too, or this call is refused a third time, harnx will end the turn.",
            count = self.count,
            tool = self.tool,
            minutes = window_minutes(),
        )
    }
}

/// How every note starts, so [`strip_note`] can recognise one.
const NOTE_PREFIX: &str = "[harnx] Same call and same result ";

/// The note appended to an executed call's result when it repeats.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepeatNote {
    pub count: usize,
}

impl RepeatNote {
    pub fn text(&self) -> String {
        let count = self.count;
        let minutes = window_minutes();
        if count >= TOOL_REPEAT_LIMIT {
            format!(
                "{NOTE_PREFIX}{count} times in the last {minutes} minutes; harnx will refuse the \
                 next identical call. Wait between checks or do something else."
            )
        } else {
            format!(
                "{NOTE_PREFIX}{count} times in the last {minutes} minutes; harnx allows \
                 {TOOL_REPEAT_LIMIT}. Wait between checks or do something else."
            )
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRepeatVerdict {
    Run,
    Refuse(Refusal),
    Stop(RepetitionTerminal),
}

/// Per-turn state. It only ever sees the calls of the current tool loop.
#[derive(Debug, Default)]
pub struct ToolRepeatGuard {
    executions: HashMap<CallKey, Vec<Execution>>,
    refusals: HashMap<CallKey, Vec<DateTime<Utc>>>,
    // A call of the current response was refused.
    refused_in_response: bool,
    // The same for the previous response; `begin_batch` moves it over.
    previous_response_refused: bool,
    compaction_marker: Option<usize>,
}

impl ToolRepeatGuard {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Call once at the start of each tool round. A user or parent message, or
    /// a compaction since the last round, starts the count over: the model has
    /// new instructions, or no longer has the earlier results in its context.
    pub fn begin_round(&mut self, user_message_arrived: bool, compaction_marker: usize) {
        let compacted = self
            .compaction_marker
            .is_some_and(|seen| seen != compaction_marker);
        if user_message_arrived || compacted {
            self.reset();
        }
        self.compaction_marker = Some(compaction_marker);
    }

    /// Call once per model response, before deciding its calls. A model sends
    /// all of a response's calls before it sees any of their results, and gets
    /// the results back together. So a refusal is "right after a refusal" only
    /// across responses: when the previous response contained a refusal,
    /// wherever it sat among that response's calls. Two refusals within one
    /// response are not in a row.
    pub fn begin_batch(&mut self) {
        self.previous_response_refused = std::mem::take(&mut self.refused_in_response);
    }

    /// Decide one requested call, in the order the model requested them. A
    /// call over the limit is refused, or ends the turn when the previous
    /// response contained a refusal (see [`Self::begin_batch`]) or when this
    /// would be the call's third refusal in the window.
    pub fn decide(&mut self, call: &ToolCall, now: DateTime<Utc>) -> ToolRepeatVerdict {
        let key = CallKey::of(call);
        let matching = self.matching_executions(&key, now);
        if matching.len() < TOOL_REPEAT_LIMIT {
            return ToolRepeatVerdict::Run;
        }
        let refusals = self.refusals_in_window(&key, now);
        if self.previous_response_refused || refusals + 1 >= REFUSALS_BEFORE_STOP {
            return ToolRepeatVerdict::Stop(RepetitionTerminal::tool_calls(
                &call.name,
                matching.len(),
            ));
        }
        self.refusals.entry(key).or_default().push(now);
        self.refused_in_response = true;
        // The count drops under the limit once this many of the oldest
        // matching calls have left the window.
        let retry_after = matching[matching.len() - TOOL_REPEAT_LIMIT] + window();
        ToolRepeatVerdict::Refuse(Refusal {
            tool: call.name.clone(),
            count: matching.len(),
            retry_after,
        })
    }

    /// Record an executed call with its result as the tool returned it, before
    /// any note is added. Returns a note when the result repeats.
    pub fn record(
        &mut self,
        call: &ToolCall,
        output: &Value,
        now: DateTime<Utc>,
    ) -> Option<RepeatNote> {
        let result = fingerprint(output);
        let executions = self.executions.entry(CallKey::of(call)).or_default();
        executions.retain(|execution| now - execution.at < window());
        executions.push(Execution { at: now, result });
        let count = executions
            .iter()
            .filter(|execution| execution.result == result)
            .count();
        (count >= 2).then_some(RepeatNote { count })
    }

    /// Times of executions in the window whose result matches the most recent one.
    fn matching_executions(&mut self, key: &CallKey, now: DateTime<Utc>) -> Vec<DateTime<Utc>> {
        let Some(executions) = self.executions.get_mut(key) else {
            return Vec::new();
        };
        executions.retain(|execution| now - execution.at < window());
        let Some(last) = executions.last().map(|execution| execution.result) else {
            return Vec::new();
        };
        executions
            .iter()
            .filter(|execution| execution.result == last)
            .map(|execution| execution.at)
            .collect()
    }

    fn refusals_in_window(&mut self, key: &CallKey, now: DateTime<Utc>) -> usize {
        let Some(times) = self.refusals.get_mut(key) else {
            return 0;
        };
        times.retain(|at| now - *at < window());
        times.len()
    }
}

/// Put `note` where the model will read it without changing the result's
/// shape: a text block for MCP results, a `harnx_note` field for other
/// objects, appended text for strings. Other shapes get no note.
pub fn append_note(output: &mut Value, note: &str) {
    match output {
        Value::Object(map) => match map.get_mut("content") {
            Some(Value::Array(blocks)) => blocks.push(json!({"type": "text", "text": note})),
            _ => {
                map.insert("harnx_note".to_string(), Value::String(note.to_string()));
            }
        },
        Value::String(text) => {
            text.push_str("\n\n");
            text.push_str(note);
        }
        _ => {}
    }
}

/// Undo [`append_note`]: return `output` as the tool returned it. A session
/// recorded with the guard on stores each result with its note, and the
/// note's count differs from one result to the next, so a replay has to
/// remove it before comparing results. A result without a note comes back
/// unchanged.
pub fn strip_note(output: &Value) -> Value {
    let mut output = output.clone();
    match &mut output {
        Value::Object(map) => match map.get_mut("content") {
            Some(Value::Array(blocks)) => strip_note_block(blocks),
            _ => strip_note_field(map),
        },
        Value::String(text) => strip_note_text(text),
        _ => {}
    }
    output
}

fn is_note(text: &str) -> bool {
    text.starts_with(NOTE_PREFIX)
}

// Only a block exactly like the one `append_note` pushes is a note; a block
// with other keys came from the tool.
fn is_note_block(block: &Value) -> bool {
    let Some(block) = block.as_object() else {
        return false;
    };
    let text = block.get("text").and_then(Value::as_str);
    block.len() == 2
        && block.get("type").and_then(Value::as_str) == Some("text")
        && text.is_some_and(is_note)
}

fn strip_note_block(blocks: &mut Vec<Value>) {
    if blocks.last().is_some_and(is_note_block) {
        blocks.pop();
    }
}

fn strip_note_field(map: &mut serde_json::Map<String, Value>) {
    if map
        .get("harnx_note")
        .and_then(Value::as_str)
        .is_some_and(is_note)
    {
        map.shift_remove("harnx_note");
    }
}

fn strip_note_text(text: &mut String) {
    if let Some(start) = text.rfind(&format!("\n\n{NOTE_PREFIX}")) {
        text.truncate(start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall::new(name.to_string(), arguments, Some("id".to_string()), None)
    }

    fn read() -> ToolCall {
        call(
            "fs_read",
            json!({"path": "a.rs", "offset": 70, "limit": 70}),
        )
    }

    /// Send `call` as a response of its own at `at` and record `output` if it
    /// runs, returning the verdict and note.
    fn run(
        guard: &mut ToolRepeatGuard,
        call: &ToolCall,
        output: &Value,
        at: i64,
    ) -> (ToolRepeatVerdict, Option<RepeatNote>) {
        let verdict = ask(guard, call, at);
        let note = matches!(verdict, ToolRepeatVerdict::Run)
            .then(|| guard.record(call, output, t(at)))
            .flatten();
        (verdict, note)
    }

    /// Send `call` as a response of its own at `at`, without a result.
    fn ask(guard: &mut ToolRepeatGuard, call: &ToolCall, at: i64) -> ToolRepeatVerdict {
        guard.begin_batch();
        guard.decide(call, t(at))
    }

    /// One response asking for `calls` in order, all decided before any result
    /// or refusal comes back.
    fn respond(
        guard: &mut ToolRepeatGuard,
        calls: &[&ToolCall],
        at: i64,
    ) -> Vec<ToolRepeatVerdict> {
        guard.begin_batch();
        calls.iter().map(|call| guard.decide(call, t(at))).collect()
    }

    /// A guard that has seen each of `calls` run four times with the same
    /// result, so the next identical request is over the limit.
    fn saturated(calls: &[&ToolCall]) -> ToolRepeatGuard {
        let mut guard = ToolRepeatGuard::default();
        for repeated in calls {
            for i in 0..4 {
                run(&mut guard, repeated, &json!("same"), i);
            }
        }
        guard
    }

    #[test]
    fn notes_on_second_to_fourth_then_refuses_the_fifth_then_stops() {
        let mut guard = ToolRepeatGuard::default();
        let out = json!({"content": [{"type": "text", "text": "70: fn x() {}"}]});
        let notes: Vec<Option<usize>> = (0..4)
            .map(|i| {
                let (verdict, note) = run(&mut guard, &read(), &out, i * 2);
                assert_eq!(verdict, ToolRepeatVerdict::Run);
                note.map(|n| n.count)
            })
            .collect();
        assert_eq!(notes, vec![None, Some(2), Some(3), Some(4)]);
        match ask(&mut guard, &read(), 8) {
            ToolRepeatVerdict::Refuse(refusal) => {
                assert_eq!(refusal.count, 4);
                assert_eq!(refusal.retry_after, t(600)); // oldest counted call leaves the window
            }
            other => panic!("expected refusal, got {other:?}"),
        }
        assert_eq!(
            ask(&mut guard, &read(), 10),
            ToolRepeatVerdict::Stop(RepetitionTerminal::tool_calls("fs_read", 4))
        );
    }

    #[test]
    fn argument_key_order_does_not_create_a_new_call() {
        let mut guard = ToolRepeatGuard::default();
        let out = json!("same");
        let orders = [
            r#"{"limit":70,"offset":70,"path":"a.rs"}"#,
            r#"{"path":"a.rs","limit":70,"offset":70}"#,
            r#"{"offset":70,"path":"a.rs","limit":70}"#,
            r#"{"limit":70,"path":"a.rs","offset":70}"#,
        ];
        for (i, order) in orders.iter().enumerate() {
            run(
                &mut guard,
                &call("fs_read", serde_json::from_str(order).unwrap()),
                &out,
                i as i64,
            );
        }
        assert!(matches!(
            ask(&mut guard, &read(), 5),
            ToolRepeatVerdict::Refuse(_)
        ));
    }

    #[test]
    fn changed_results_or_arguments_never_count() {
        let mut guard = ToolRepeatGuard::default();
        for i in 0..10 {
            let (verdict, note) = run(&mut guard, &read(), &json!({"lines": i}), i);
            assert_eq!(verdict, ToolRepeatVerdict::Run);
            assert!(note.is_none(), "changing results must not be noted");
        }
        for i in 0..10 {
            let other = call("fs_read", json!({"path": "a.rs", "offset": i}));
            assert_eq!(
                run(&mut guard, &other, &json!("x"), 20 + i).0,
                ToolRepeatVerdict::Run
            );
        }
    }

    #[test]
    fn the_call_runs_again_once_the_window_clears() {
        let mut guard = ToolRepeatGuard::default();
        for i in 0..4 {
            run(&mut guard, &read(), &json!("same"), i);
        }
        assert!(matches!(
            ask(&mut guard, &read(), 5),
            ToolRepeatVerdict::Refuse(_)
        ));
        // Something else runs, so the stop condition resets; then wait past the window.
        run(&mut guard, &call("fs_ls", json!({})), &json!([]), 6);
        assert_eq!(ask(&mut guard, &read(), 601), ToolRepeatVerdict::Run);
    }

    #[test]
    fn one_premature_retry_is_forgiven_and_the_third_refusal_stops() {
        let mut guard = ToolRepeatGuard::default();
        for i in 0..4 {
            run(&mut guard, &read(), &json!("same"), i);
        }
        assert!(matches!(
            ask(&mut guard, &read(), 10),
            ToolRepeatVerdict::Refuse(_)
        ));
        run(&mut guard, &call("fs_ls", json!({})), &json!([1]), 20);
        assert!(matches!(
            ask(&mut guard, &read(), 30),
            ToolRepeatVerdict::Refuse(_)
        ));
        run(&mut guard, &call("fs_ls", json!({})), &json!([2]), 40);
        assert!(matches!(
            ask(&mut guard, &read(), 50),
            ToolRepeatVerdict::Stop(_)
        ));
    }

    #[test]
    fn a_loop_alternating_a_refused_call_with_a_changing_call_stops() {
        let mut guard = ToolRepeatGuard::default();
        for i in 0..4 {
            run(&mut guard, &read(), &json!("same"), i);
        }
        let mut stopped = false;
        for i in 0..6 {
            run(
                &mut guard,
                &call("time_get_current_time", json!({})),
                &json!(i),
                10 + i * 2,
            );
            if matches!(
                ask(&mut guard, &read(), 11 + i * 2),
                ToolRepeatVerdict::Stop(_)
            ) {
                stopped = true;
                break;
            }
        }
        assert!(stopped, "the third refusal of the read must end the turn");
    }

    #[test]
    fn the_third_refusal_rule_counts_each_call_separately() {
        let mut guard = ToolRepeatGuard::default();
        let saturated: Vec<ToolCall> = ["a.rs", "b.rs", "c.rs"]
            .into_iter()
            .map(|path| call("fs_read", json!({ "path": path })))
            .collect();
        for (i, repeated) in saturated.iter().enumerate() {
            for j in 0..4 {
                run(&mut guard, repeated, &json!("same"), (i * 4 + j) as i64);
            }
        }
        // Three refusals in the window, but each is the first of its call.
        for (i, repeated) in saturated.iter().enumerate() {
            let at = 20 + 2 * i as i64;
            let verdict = ask(&mut guard, repeated, at);
            assert!(
                matches!(verdict, ToolRepeatVerdict::Refuse(_)),
                "{}: {verdict:?}",
                repeated.arguments
            );
            let clock = call("time_get_current_time", json!({}));
            run(&mut guard, &clock, &json!(at), at + 1);
        }
    }

    #[test]
    fn over_limit_calls_in_one_response_are_refused_and_the_next_response_stops() {
        let a = read();
        let b = call("fs_read", json!({"path": "b.rs"}));
        for asked_again in [&a, &b] {
            let mut guard = ToolRepeatGuard::default();
            for i in 0..4 {
                run(&mut guard, &a, &json!("a"), i);
                run(&mut guard, &b, &json!("b"), i);
            }
            // The model sent both calls before it had seen either refusal.
            guard.begin_batch();
            let verdicts = [guard.decide(&a, t(10)), guard.decide(&b, t(10))];
            assert!(
                verdicts
                    .iter()
                    .all(|verdict| matches!(verdict, ToolRepeatVerdict::Refuse(_))),
                "{verdicts:?}"
            );
            // Its next response asks again after seeing them.
            assert_eq!(
                ask(&mut guard, asked_again, 12),
                ToolRepeatVerdict::Stop(RepetitionTerminal::tool_calls("fs_read", 4))
            );
        }
    }

    #[test]
    fn a_refusal_anywhere_in_the_previous_response_ends_the_turn() {
        let x = read();
        let y = call("fs_ls", json!({}));
        // The model sees the refusal wherever its call sat in the response, so
        // the order of the calls must not change the outcome.
        for (refused, response) in [("first", [&x, &y]), ("last", [&y, &x])] {
            let mut guard = saturated(&[&x]);
            let verdicts = respond(&mut guard, &response, 10);
            let refusals = verdicts
                .iter()
                .filter(|verdict| matches!(verdict, ToolRepeatVerdict::Refuse(_)))
                .count();
            assert_eq!(refusals, 1, "refused {refused}: {verdicts:?}");
            assert_eq!(
                ask(&mut guard, &x, 12),
                ToolRepeatVerdict::Stop(RepetitionTerminal::tool_calls("fs_read", 4)),
                "refused {refused}"
            );
        }
    }

    #[test]
    fn a_refusal_in_the_previous_response_ends_the_turn_for_any_other_call_too() {
        let x = read();
        let z = call("fs_read", json!({"path": "z.rs"}));
        let mut guard = saturated(&[&x, &z]);
        // Only x is refused, and a call that ran comes after it.
        let verdicts = respond(&mut guard, &[&x, &call("fs_ls", json!({}))], 10);
        assert!(
            matches!(verdicts[0], ToolRepeatVerdict::Refuse(_)),
            "{verdicts:?}"
        );
        // z is over the limit too, and this would be its first refusal.
        assert_eq!(
            ask(&mut guard, &z, 12),
            ToolRepeatVerdict::Stop(RepetitionTerminal::tool_calls("fs_read", 4))
        );
    }

    #[test]
    fn one_call_sent_twice_in_a_response_is_refused_twice() {
        let mut guard = ToolRepeatGuard::default();
        for i in 0..4 {
            run(&mut guard, &read(), &json!("same"), i);
        }
        let copy = ToolCall {
            id: Some("copy".to_string()),
            ..read()
        };
        guard.begin_batch();
        let verdicts = [guard.decide(&read(), t(10)), guard.decide(&copy, t(10))];
        assert!(
            verdicts
                .iter()
                .all(|verdict| matches!(verdict, ToolRepeatVerdict::Refuse(_))),
            "{verdicts:?}"
        );
        // A third copy is the call's third refusal, which ends the turn even
        // within one response.
        assert!(matches!(
            guard.decide(&read(), t(10)),
            ToolRepeatVerdict::Stop(_)
        ));
    }

    #[test]
    fn an_a_b_alternation_is_refused_on_as_fifth_request() {
        let mut guard = ToolRepeatGuard::default();
        let a = read();
        let b = call("fs_read", json!({"path": "b.rs"}));
        for i in 0..4 {
            assert_eq!(
                run(&mut guard, &a, &json!("a"), i * 2).0,
                ToolRepeatVerdict::Run
            );
            assert_eq!(
                run(&mut guard, &b, &json!("b"), i * 2 + 1).0,
                ToolRepeatVerdict::Run
            );
        }
        assert!(matches!(
            ask(&mut guard, &a, 9),
            ToolRepeatVerdict::Refuse(_)
        ));
    }

    #[test]
    fn user_messages_and_compaction_reset_the_guard() {
        let mut guard = ToolRepeatGuard::default();
        guard.begin_round(false, 0);
        for i in 0..4 {
            run(&mut guard, &read(), &json!("same"), i);
        }
        guard.begin_round(true, 0);
        assert_eq!(ask(&mut guard, &read(), 5), ToolRepeatVerdict::Run);

        let mut guard = ToolRepeatGuard::default();
        guard.begin_round(false, 3);
        for i in 0..4 {
            run(&mut guard, &read(), &json!("same"), i);
        }
        guard.begin_round(false, 7); // compaction moved messages into compressed_messages
        assert_eq!(ask(&mut guard, &read(), 5), ToolRepeatVerdict::Run);
    }

    #[test]
    fn a_round_with_no_reset_trigger_keeps_the_count() {
        let mut guard = ToolRepeatGuard::default();
        guard.begin_round(false, 3);
        for i in 0..4 {
            run(&mut guard, &read(), &json!("same"), i);
        }
        // begin_round runs at the start of every tool round. A round with
        // neither a user message nor a compaction must leave the count alone,
        // or the guard would forget the repeats before it could act on them.
        guard.begin_round(false, 3);
        let verdict = ask(&mut guard, &read(), 5);
        assert!(
            matches!(verdict, ToolRepeatVerdict::Refuse(_)),
            "expected a refusal, got {verdict:?}"
        );
    }

    #[test]
    fn identical_calls_in_one_round_both_run_and_both_count() {
        let mut guard = ToolRepeatGuard::default();
        assert_eq!(guard.decide(&read(), t(0)), ToolRepeatVerdict::Run);
        assert_eq!(guard.decide(&read(), t(0)), ToolRepeatVerdict::Run);
        assert!(guard.record(&read(), &json!("same"), t(1)).is_none());
        assert_eq!(
            guard.record(&read(), &json!("same"), t(1)).map(|n| n.count),
            Some(2)
        );
    }

    #[test]
    fn note_and_refusal_text_match_the_spec() {
        assert_eq!(
            RepeatNote { count: 2 }.text(),
            "[harnx] Same call and same result 2 times in the last 10 minutes; harnx allows 4. \
             Wait between checks or do something else."
        );
        assert_eq!(
            RepeatNote { count: 4 }.text(),
            "[harnx] Same call and same result 4 times in the last 10 minutes; harnx will refuse \
             the next identical call. Wait between checks or do something else."
        );
        let refusal = Refusal {
            tool: "fs_read".into(),
            count: 4,
            retry_after: t(600),
        };
        let message = refusal.message();
        assert!(message.starts_with(
            "harnx did not run this call. It matches your last 4 calls to `fs_read` in the past \
             10 minutes, and each returned the same result. It can run again after "
        ));
        assert!(message.ends_with(
            "Use the result you have, wait, or take a different approach. If your next call is \
             refused too, or this call is refused a third time, harnx will end the turn."
        ));
    }

    fn local_time(at: DateTime<Utc>) -> String {
        at.with_timezone(&Local)
            .format("%Y-%m-%dT%H:%M:%S%:z")
            .to_string()
    }

    #[test]
    fn a_sub_second_retry_time_is_shown_rounded_up() {
        let mut guard = ToolRepeatGuard::default();
        let oldest = t(0) + TimeDelta::milliseconds(250);
        guard.record(&read(), &json!("same"), oldest);
        for i in 1..4 {
            guard.record(&read(), &json!("same"), t(i));
        }
        let ToolRepeatVerdict::Refuse(refusal) = ask(&mut guard, &read(), 5) else {
            panic!("the fifth identical call must be refused");
        };
        // The call may run again only once the exact time has passed, so the
        // message names the next whole second rather than the one before it.
        assert_eq!(refusal.retry_after, oldest + window());
        let message = refusal.message();
        assert!(
            message.contains(&format!("It can run again after {}.", local_time(t(601)))),
            "{message}"
        );

        let whole_second = Refusal {
            retry_after: t(600),
            ..refusal
        };
        let message = whole_second.message();
        assert!(
            message.contains(&format!("It can run again after {}.", local_time(t(600)))),
            "{message}"
        );
    }

    #[test]
    fn notes_are_placed_without_changing_the_result_shape() {
        let mut mcp = json!({"content": [{"type": "text", "text": "body"}], "isError": false});
        append_note(&mut mcp, "N");
        assert_eq!(mcp["content"][1], json!({"type": "text", "text": "N"}));
        assert_eq!(mcp["isError"], json!(false));

        let mut object = json!({"message": "Waited 60.0 seconds"});
        append_note(&mut object, "N");
        assert_eq!(
            object,
            json!({"message": "Waited 60.0 seconds", "harnx_note": "N"})
        );

        let mut text = json!("output");
        append_note(&mut text, "N");
        assert_eq!(text, json!("output\n\nN"));

        for mut other in [json!([1, 2]), Value::Null, json!(3)] {
            let before = other.clone();
            append_note(&mut other, "N");
            assert_eq!(other, before);
        }
    }

    #[test]
    fn strip_note_removes_exactly_what_append_note_added() {
        let note = RepeatNote { count: 3 }.text();
        for original in [
            json!({"content": [{"type": "text", "text": "body"}], "isError": false}),
            json!({"message": "Waited 60.0 seconds"}),
            json!("output"),
        ] {
            let mut noted = original.clone();
            append_note(&mut noted, &note);
            assert_ne!(noted, original);
            assert_eq!(strip_note(&noted), original);
        }
    }

    #[test]
    fn strip_note_leaves_results_without_a_note_alone() {
        let prefix = "[harnx] Same call and same result 3 times";
        for unnoted in [
            json!({"content": [{"type": "text", "text": "body"}]}),
            // A block with more than the note's two keys is the tool's own.
            json!({"content": [{"type": "text", "text": prefix, "annotations": {}}]}),
            json!({"message": "x", "harnx_note": "a field the tool set itself"}),
            json!("output"),
            json!("output\n\n[harnx] something else"),
            json!([prefix]),
            Value::Null,
        ] {
            assert_eq!(strip_note(&unnoted), unnoted);
        }
    }
}
