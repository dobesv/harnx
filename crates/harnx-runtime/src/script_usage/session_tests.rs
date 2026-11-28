use chrono::{DateTime, TimeZone, Utc};
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::{SessionLogEntry, ToolOutput};
use harnx_core::tool::ToolCall;
use serde_json::{json, Value};

use super::session::*;

fn at(h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 20, h, 0, 0).unwrap()
}

fn tool_calls(at: Option<DateTime<Utc>>, calls: &[(Option<&str>, &str, Value)]) -> SessionLogEntry {
    SessionLogEntry::ToolCalls {
        text: String::new(),
        thought: None,
        calls: calls
            .iter()
            .map(|(id, name, args)| ToolCall {
                name: (*name).to_string(),
                arguments: args.clone(),
                id: id.map(str::to_string),
                thought_signature: None,
                reasoning_provenance: None,
            })
            .collect(),
        timestamp: at,
        fence_token: None,
    }
}

fn output(id: Option<&str>, name: &str, output: Value) -> ToolOutput {
    ToolOutput {
        id: id.map(str::to_string),
        name: name.to_string(),
        output,
        markdown: None,
        content: vec![],
        user_surfaces: vec![],
        switch_agent: None,
    }
}

fn tool_results(results: Vec<ToolOutput>) -> SessionLogEntry {
    SessionLogEntry::ToolResults {
        results,
        handoff_selection: None,
        timestamp: None,
    }
}

fn turn_end(at: Option<DateTime<Utc>>, usage: Option<CompletionTokenUsage>) -> SessionLogEntry {
    SessionLogEntry::TurnEnd {
        through_seq: 0,
        fence_token: 0,
        timestamp: at,
        usage,
    }
}

fn cancel() -> SessionLogEntry {
    SessionLogEntry::Cancel {
        fence_token: 0,
        cancellation_id: None,
        requested_by: None,
        timestamp: None,
    }
}

fn user_message() -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("hi".into()),
        timestamp: None,
        fence_token: None,
    }
}

fn compress() -> SessionLogEntry {
    SessionLogEntry::Compress {
        prompt: "summary".into(),
    }
}

fn numbered(entries: Vec<SessionLogEntry>) -> Vec<(u64, SessionLogEntry)> {
    entries
        .into_iter()
        .enumerate()
        .map(|(i, e)| (i as u64 + 1, e))
        .collect()
}

fn stderr_report(stderr: &str) -> String {
    format!("exit 1\n<!-- start stderr -->\n{stderr}<!-- end stderr -->\n")
}

fn report(trace: &str) -> String {
    format!("exit 0\n<!-- start stderr -->\n<!-- end stderr -->\n{trace}")
}

fn payload(status: &str, exit_code: i64, text: &str) -> Value {
    json!({
        "status": status,
        "exit_code": exit_code,
        "stdout_truncated": false,
        "stderr_truncated": false,
        "limit_hit": null,
        "content": [{"type": "text", "text": text}],
    })
}

fn bash_run(out: Value) -> Vec<ScriptRun> {
    let entries = numbered(vec![
        tool_calls(None, &[(Some("a"), "bash", json!({"script": "echo hi"}))]),
        tool_results(vec![output(Some("a"), "bash", out)]),
        turn_end(None, None),
    ]);
    analyze_session(&entries, &TimeWindow::default()).scripts
}

#[test]
fn a_successful_script_reports_its_inner_calls() {
    let text = report("## inner call trace\n- fs_read ok 120B 4ms\n- fs_grep ok 80B 9ms\n");
    let entries = numbered(vec![
        tool_calls(
            Some(at(1)),
            &[(Some("a"), "bash", json!({"script": "cat x | grep y"}))],
        ),
        tool_results(vec![output(Some("a"), "bash", payload("ok", 0, &text))]),
        turn_end(Some(at(1)), None),
    ]);
    let usage = analyze_session(&entries, &TimeWindow::default());
    assert_eq!(usage.scripts.len(), 1);
    let run = &usage.scripts[0];
    assert_eq!(run.seq, 1);
    assert_eq!(run.failure, None);
    assert_eq!(run.status, "ok");
    assert_eq!(run.inner.len(), 2);
    assert_eq!(run.inner[0].name, "fs_read");
    assert_eq!(run.script_bytes, "cat x | grep y".len() as u64);
    assert!(run.result_bytes > 0);
}

#[test]
fn a_failed_script_is_read_from_its_partial_result() {
    let mut inner = payload("error", 1, &report(""));
    inner["failed_call"] =
        json!({"name": "fs_read", "status": "tool_error", "args_summary": "x", "call_id": "c"});
    let runs = bash_run(json!({"is_error": true, "error": "boom", "partial_result": inner}));
    assert_eq!(runs[0].failure, Some(FailureKind::InnerCallFailed));
    assert_eq!(
        runs[0].failed_call,
        Some(FailedCall {
            name: "fs_read".into(),
            status: "tool_error".into()
        })
    );
    assert_eq!(runs[0].exit_code, Some(1));
}

#[test]
fn an_older_payload_uses_its_trace_array() {
    let mut p = payload("ok", 0, &report(""));
    p["trace"] = json!([
        {"name": "fs_read", "status": "ok", "bytes": 10, "duration_ms": 2},
        {"name": 5}
    ]);
    p["trace_dropped"] = json!(3);
    let runs = bash_run(p);
    assert_eq!(runs[0].inner.len(), 2);
    assert_eq!(runs[0].inner[0].bytes, 10);
    assert_eq!(runs[0].inner[0].count, 1);
    assert_eq!(runs[0].inner[1].name, "");
    assert_eq!(runs[0].trace_dropped, 3);
}

#[test]
fn a_round_without_results_is_no_result() {
    let entries = numbered(vec![tool_calls(
        None,
        &[(Some("a"), "bash", json!({"script": "x"}))],
    )]);
    let usage = analyze_session(&entries, &TimeWindow::default());
    assert_eq!(usage.scripts[0].status, "no_result");
    assert_eq!(usage.scripts[0].failure, Some(FailureKind::NoResult));
    assert_eq!(usage.turns.len(), 1);
    assert!(!usage.turns[0].finished);
    assert_eq!(usage.turns[0].rounds, 1);
}

#[test]
fn failure_kinds_follow_the_documented_order() {
    let tb = stderr_report("Traceback (most recent call last):\n  File x\nValueError: no\n");
    let mut limit = payload("error", 1, &report(""));
    limit["limit_hit"] = json!("memory");
    let mut both = payload("error", 1, &tb);
    both["failed_call"] = json!({"name": "t", "status": "tool_error"});
    let mut limit_and_failed = limit.clone();
    limit_and_failed["failed_call"] = json!({"name": "t", "status": "tool_error"});
    let mut interrupted = json!({"status": "interrupted", "error": "worker restarted"});
    interrupted["content"] = json!([{"type": "text", "text": "x"}]);
    let cases: Vec<(Value, Option<FailureKind>)> = vec![
        (payload("ok", 0, &report("")), None),
        (interrupted, Some(FailureKind::Interrupted)),
        (limit_and_failed, Some(FailureKind::LimitHit)),
        (
            payload("timed_out", 1, &report("")),
            Some(FailureKind::TimedOut),
        ),
        (
            payload("cancelled", 1, &report("")),
            Some(FailureKind::Cancelled),
        ),
        (
            payload("stopped", 1, &report("")),
            Some(FailureKind::Stopped),
        ),
        (limit.clone(), Some(FailureKind::LimitHit)),
        (
            payload("error", 126, &report("")),
            Some(FailureKind::Exit126),
        ),
        (
            payload("error", 127, &report("")),
            Some(FailureKind::Exit127),
        ),
        (both, Some(FailureKind::InnerCallFailed)),
        (
            payload("error", 1, &stderr_report("TypeError: bad\n")),
            Some(FailureKind::PythonTypeError),
        ),
        (payload("error", 1, &tb), Some(FailureKind::PythonError)),
        (
            payload("error", 2, &report("")),
            Some(FailureKind::NonZeroExit),
        ),
        (
            payload("ok", 3, &report("")),
            Some(FailureKind::NonZeroExit),
        ),
        (
            json!({"is_error": true, "error": "x"}),
            Some(FailureKind::NonZeroExit),
        ),
        (json!("plain"), None),
    ];
    assert_eq!(bash_run(json!("plain"))[0].status, "unknown");
    for (out, want) in cases {
        let runs = bash_run(out.clone());
        assert_eq!(runs[0].failure, want, "{out}");
    }
}

#[test]
fn direct_calls_are_counted_per_tool_with_errors_and_bytes() {
    let entries = numbered(vec![
        tool_calls(
            None,
            &[
                (Some("a"), "fs_read", json!({"path": "x"})),
                (Some("b"), "fs_read", json!({"path": "y"})),
                (Some("c"), "time_now", json!({})),
            ],
        ),
        tool_results(vec![
            output(Some("a"), "fs_read", json!({"text": "hello"})),
            output(
                Some("b"),
                "fs_read",
                json!({"is_error": true, "error": "no"}),
            ),
            output(Some("c"), "time_now", json!("t")),
        ]),
        turn_end(None, None),
    ]);
    let usage = analyze_session(&entries, &TimeWindow::default());
    let read = &usage.direct["fs_read"];
    assert_eq!(read.calls, 2);
    assert_eq!(read.errors, 1);
    assert_eq!(read.args_bytes, 2 * r#"{"path":"x"}"#.len() as u64);
    assert_eq!(
        read.result_bytes,
        (r#"{"text":"hello"}"#.len() + r#"{"is_error":true,"error":"no"}"#.len()) as u64
    );
    assert_eq!(usage.direct["time_now"].errors, 0);
    assert!(usage.scripts.is_empty());
}

#[test]
fn turns_group_rounds_and_carry_usage() {
    let used = CompletionTokenUsage {
        input_tokens: 10,
        output_tokens: 2,
        cached_tokens: 0,
        cache_write_tokens: 0,
    };
    let entries = numbered(vec![
        tool_calls(None, &[(Some("a"), "bash", json!({"script": "x"}))]),
        tool_results(vec![output(
            Some("a"),
            "bash",
            payload("ok", 0, &report("")),
        )]),
        turn_end(None, Some(used.clone())),
        tool_calls(None, &[(Some("b"), "fs_read", json!({}))]),
        tool_results(vec![output(Some("b"), "fs_read", json!("x"))]),
        tool_calls(None, &[(Some("c"), "fs_read", json!({}))]),
        tool_results(vec![output(Some("c"), "fs_read", json!("x"))]),
        turn_end(None, None),
        tool_calls(None, &[(Some("d"), "fs_read", json!({}))]),
        cancel(),
    ]);
    let usage = analyze_session(&entries, &TimeWindow::default());
    assert_eq!(usage.turns.len(), 3);
    let t = &usage.turns;
    assert!(t[0].finished && t[0].rounds == 1 && t[0].scripts == 1 && t[0].direct_calls == 0);
    assert_eq!(t[0].usage, Some(used));
    assert!(t[1].finished && t[1].rounds == 2 && t[1].scripts == 0 && t[1].direct_calls == 2);
    assert!(!t[2].finished && t[2].rounds == 1 && t[2].direct_calls == 1);
}

#[test]
fn a_turn_outside_the_window_contributes_nothing() {
    let entries = numbered(vec![
        tool_calls(Some(at(1)), &[(Some("a"), "bash", json!({"script": "x"}))]),
        tool_results(vec![output(
            Some("a"),
            "bash",
            payload("ok", 0, &report("")),
        )]),
        turn_end(Some(at(1)), None),
        tool_calls(Some(at(5)), &[(Some("b"), "fs_read", json!({}))]),
        tool_results(vec![output(Some("b"), "fs_read", json!("x"))]),
        turn_end(Some(at(5)), None),
    ]);
    let window = TimeWindow {
        since: Some(at(3)),
        until: Some(at(8)),
    };
    let usage = analyze_session(&entries, &window);
    assert!(usage.scripts.is_empty());
    assert_eq!(usage.turns.len(), 1);
    assert_eq!(usage.direct["fs_read"].calls, 1);
    assert!(!usage.direct.contains_key("bash"));
    // until is exclusive
    let window = TimeWindow {
        since: None,
        until: Some(at(5)),
    };
    assert_eq!(analyze_session(&entries, &window).turns.len(), 1);
}

#[test]
fn entries_without_timestamps_are_inside_any_window() {
    let entries = numbered(vec![
        tool_calls(None, &[(Some("a"), "fs_read", json!({}))]),
        tool_results(vec![output(Some("a"), "fs_read", json!("x"))]),
        turn_end(None, None),
    ]);
    let window = TimeWindow {
        since: Some(at(3)),
        until: Some(at(4)),
    };
    assert_eq!(analyze_session(&entries, &window).turns.len(), 1);
    assert!(window.contains(None));
}

#[test]
fn results_match_calls_by_id_when_order_differs() {
    let entries = numbered(vec![
        tool_calls(
            None,
            &[
                (Some("a"), "bash", json!({"script": "x"})),
                (Some("b"), "fs_read", json!({})),
            ],
        ),
        tool_results(vec![
            output(Some("b"), "fs_read", json!({"is_error": true})),
            output(Some("a"), "bash", payload("ok", 0, &report(""))),
        ]),
        turn_end(None, None),
    ]);
    let usage = analyze_session(&entries, &TimeWindow::default());
    assert_eq!(usage.scripts[0].status, "ok");
    assert_eq!(usage.direct["fs_read"].errors, 1);
}

#[test]
fn unrelated_entries_are_ignored() {
    let entries = numbered(vec![SessionLogEntry::Clear, turn_end(None, None)]);
    let usage = analyze_session(&entries, &TimeWindow::default());
    assert_eq!(usage.turns.len(), 1);
    assert_eq!(usage.turns[0].rounds, 0);
}

#[test]
fn a_script_that_exits_zero_never_has_a_failure() {
    let traceback = stderr_report("Traceback (most recent call last):\n  File x\nValueError: no\n");
    let mut guarded = payload("ok", 0, &report(""));
    guarded["failed_call"] = json!({"name": "fs_read", "status": "tool_error"});
    let cases = [
        payload("ok", 0, &traceback),
        payload("ok", 0, &stderr_report("TypeError: bad\n")),
        guarded,
    ];
    for out in cases {
        let runs = bash_run(out.clone());
        assert_eq!(runs[0].failure, None, "{out}");
    }
    let mut guarded = payload("ok", 0, &report(""));
    guarded["failed_call"] = json!({"name": "fs_read", "status": "tool_error"});
    assert_eq!(
        bash_run(guarded)[0].failed_call.as_ref().unwrap().name,
        "fs_read"
    );
}

fn bash_round(id: &str, at: Option<DateTime<Utc>>) -> [SessionLogEntry; 2] {
    [
        tool_calls(at, &[(Some(id), "bash", json!({"script": "x"}))]),
        tool_results(vec![output(
            Some(id),
            "bash",
            payload("ok", 0, &report("")),
        )]),
    ]
}

#[test]
fn a_compaction_copy_inside_a_turn_is_not_counted_again() {
    let mut log = bash_round("a", None).to_vec();
    log.push(compress());
    log.extend(bash_round("a", None));
    log.push(turn_end(None, None));
    let usage = analyze_session(&numbered(log), &TimeWindow::default());
    assert_eq!(usage.turns.len(), 1);
    assert_eq!(usage.turns[0].rounds, 1);
    assert_eq!(usage.turns[0].scripts, 1);
    assert_eq!(usage.scripts.len(), 1);
    assert_eq!(usage.scripts[0].seq, 1);
}

#[test]
fn a_compaction_copy_at_the_start_of_a_turn_does_not_set_its_time() {
    let mut log = bash_round("a", Some(at(1))).to_vec();
    log.push(turn_end(Some(at(1)), None));
    log.push(compress());
    log.extend(bash_round("a", Some(at(1))));
    log.extend(bash_round("b", Some(at(5))));
    log.push(turn_end(Some(at(5)), None));
    let window = TimeWindow {
        since: Some(at(3)),
        until: None,
    };
    let usage = analyze_session(&numbered(log), &window);
    assert_eq!(usage.turns.len(), 1);
    assert_eq!(usage.turns[0].rounds, 1);
    assert_eq!(usage.scripts.len(), 1);
}

#[test]
fn copies_after_the_last_terminator_form_no_turn() {
    let mut log = bash_round("a", None).to_vec();
    log.push(turn_end(None, None));
    log.push(compress());
    log.extend(bash_round("a", None));
    let usage = analyze_session(&numbered(log), &TimeWindow::default());
    assert_eq!(usage.turns.len(), 1);
}

#[test]
fn rounds_without_call_ids_are_never_treated_as_copies() {
    let anonymous = || {
        [
            tool_calls(None, &[(None, "bash", json!({"script": "x"}))]),
            tool_results(vec![output(None, "bash", payload("ok", 0, &report("")))]),
        ]
    };
    let mut log = anonymous().to_vec();
    log.extend(anonymous());
    log.push(turn_end(None, None));
    let usage = analyze_session(&numbered(log), &TimeWindow::default());
    assert_eq!(usage.turns[0].rounds, 2);
    assert_eq!(usage.scripts.len(), 2);
}

#[test]
fn an_idle_cancel_is_not_a_turn() {
    let log = vec![cancel(), turn_end(None, None)];
    let usage = analyze_session(&numbered(log), &TimeWindow::default());
    assert_eq!(usage.turns.len(), 1);
    assert!(usage.turns[0].finished);
    let usage = analyze_session(&numbered(vec![cancel()]), &TimeWindow::default());
    assert!(usage.turns.is_empty());
}

#[test]
fn a_cancelled_turn_with_a_user_message_and_no_rounds_is_unfinished() {
    let log = vec![user_message(), cancel()];
    let usage = analyze_session(&numbered(log), &TimeWindow::default());
    assert_eq!(usage.turns.len(), 1);
    assert!(!usage.turns[0].finished);
    assert_eq!(usage.turns[0].rounds, 0);
}

#[test]
fn a_turn_without_rounds_takes_its_time_from_its_terminator() {
    let log = vec![user_message(), turn_end(Some(at(5)), None)];
    let window = TimeWindow {
        since: Some(at(3)),
        until: Some(at(8)),
    };
    assert_eq!(
        analyze_session(&numbered(log.clone()), &window).turns.len(),
        1
    );
    let late = TimeWindow {
        since: Some(at(6)),
        until: None,
    };
    assert!(analyze_session(&numbered(log), &late).turns.is_empty());
}

#[test]
fn entries_after_the_last_terminator_without_a_round_form_no_turn() {
    let log = vec![turn_end(None, None), user_message()];
    let usage = analyze_session(&numbered(log), &TimeWindow::default());
    assert_eq!(usage.turns.len(), 1);
}

#[test]
fn failure_kind_names_match_their_serialized_form() {
    for kind in FailureKind::ALL {
        assert_eq!(serde_json::to_value(kind).unwrap(), json!(kind.as_str()));
    }
    assert_eq!(FailureKind::Exit126.as_str(), "exit_126");
    assert_eq!(FailureKind::Exit127.as_str(), "exit_127");
}
