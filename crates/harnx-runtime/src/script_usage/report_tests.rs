use chrono::{DateTime, TimeZone, Utc};
use harnx_core::api_types::CompletionTokenUsage;

use super::report::*;
use super::session::{
    DirectToolStats, FailedCall, FailureKind, ScriptRun, SessionScriptUsage, TimeWindow, TurnFacts,
};
use super::trace::TraceRow;

fn at(h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 20, h, 0, 0).unwrap()
}

fn row(name: &str, status: &str, count: u64, bytes: u64) -> TraceRow {
    TraceRow {
        name: name.into(),
        status: status.into(),
        count,
        bytes,
        duration_ms: 1,
    }
}

fn script(seq: u64, rows: Vec<TraceRow>, dropped: u64) -> ScriptRun {
    ScriptRun {
        seq,
        timestamp: Some(at(1)),
        status: "ok".into(),
        exit_code: Some(0),
        failure: None,
        failed_call: None,
        inner: rows,
        trace_dropped: dropped,
        unparsed_trace_lines: 0,
        script_bytes: 10,
        result_bytes: 100,
        output_truncated: false,
    }
}

fn failed(seq: u64, hour: Option<u32>, kind: FailureKind) -> ScriptRun {
    ScriptRun {
        timestamp: hour.map(at),
        status: "error".into(),
        exit_code: Some(1),
        failure: Some(kind),
        ..script(seq, vec![], 0)
    }
}

fn usage_of(scripts: Vec<ScriptRun>) -> SessionScriptUsage {
    SessionScriptUsage {
        scripts,
        ..Default::default()
    }
}

fn turn(finished: bool, rounds: u64, scripts: u64, usage: Option<u64>) -> TurnFacts {
    TurnFacts {
        finished,
        rounds,
        direct_calls: 0,
        scripts,
        usage: usage.map(|n| CompletionTokenUsage {
            input_tokens: n,
            output_tokens: n * 2,
            cached_tokens: 0,
            cache_write_tokens: 0,
        }),
    }
}

fn direct(calls: u64, result_bytes: u64) -> DirectToolStats {
    DirectToolStats {
        calls,
        errors: 0,
        args_bytes: 0,
        result_bytes,
    }
}

fn builder() -> ReportBuilder {
    ReportBuilder::new("c", &ScriptUsageScope::default())
}

#[test]
fn empty_report_is_all_zeros() {
    let report = builder().finish();
    assert_eq!(report.sessions_scanned, 0);
    assert_eq!(report.sessions_with_scripts, 0);
    assert_eq!(report.sessions_unreadable, 0);
    assert_eq!(report.totals.scripts, 0);
    assert_eq!(report.totals.rounds, 0);
    assert!(report.by_agent.is_empty());
    assert!(report.tools.is_empty());
    assert!(report.failures.is_empty());
    assert_eq!(report.failures_omitted, 0);
}

#[test]
fn totals_merge_across_sessions_and_split_by_agent() {
    let mut a = usage_of(vec![script(2, vec![row("fs_read", "ok", 2, 50)], 0)]);
    a.direct.insert(
        "time_now".into(),
        DirectToolStats {
            calls: 3,
            errors: 1,
            args_bytes: 7,
            result_bytes: 70,
        },
    );
    let b = usage_of(vec![
        script(2, vec![row("fs_read", "tool_error", 1, 5)], 0),
        failed(4, Some(2), FailureKind::NonZeroExit),
    ]);
    let mut builder = builder();
    builder.add_session("pantheon/atlas", "s1", a);
    builder.add_session("coding/coder", "s2", b);
    let report = builder.finish();

    assert_eq!(report.sessions_scanned, 2);
    assert_eq!(report.sessions_with_scripts, 2);
    let t = &report.totals;
    assert_eq!((t.scripts, t.scripts_ok), (3, 2));
    assert_eq!(t.script_failures[&FailureKind::NonZeroExit], 1);
    assert_eq!(t.inner_calls, 3);
    assert_eq!(t.inner_calls_by_status["ok"], 2);
    assert_eq!(t.inner_calls_by_status["tool_error"], 1);
    assert_eq!(t.inner_result_bytes, 55);
    assert_eq!(t.script_bytes, 30);
    assert_eq!(t.script_result_bytes, 300);
    assert_eq!((t.direct_calls, t.direct_errors), (3, 1));
    assert_eq!((t.direct_args_bytes, t.direct_result_bytes), (7, 70));
    assert_eq!(report.by_agent.len(), 2);
    assert_eq!(report.by_agent["pantheon/atlas"].scripts, 1);
    assert_eq!(report.by_agent["coding/coder"].scripts, 2);
    assert_eq!(report.by_agent["coding/coder"].direct_calls, 0);
}

#[test]
fn calls_per_script_bucket_edges() {
    let scripts = [0u64, 1, 2, 5, 6, 20, 21]
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let rows = if *n == 0 {
                vec![]
            } else {
                vec![row("t", "ok", *n, 0)]
            };
            script(i as u64, rows, 0)
        })
        .chain([script(9, vec![row("t", "ok", 3, 0)], 4)])
        .collect();
    let mut builder = builder();
    builder.add_session("a", "s", usage_of(scripts));
    let b = builder.finish().totals.calls_per_script;
    assert_eq!(
        (b.zero, b.one, b.two_to_five, b.six_to_twenty, b.over_twenty),
        (1, 1, 2, 3, 1)
    );
    // The last script's 3 retained + 4 dropped calls land in 6..=20.
}

#[test]
fn dropped_calls_count_toward_the_bucket_not_inner_calls() {
    let mut builder = builder();
    builder.add_session(
        "a",
        "s",
        usage_of(vec![script(1, vec![row("t", "ok", 3, 0)], 4)]),
    );
    let t = builder.finish().totals;
    assert_eq!(t.calls_per_script.six_to_twenty, 1);
    assert_eq!(t.trace_dropped, 4);
    assert_eq!(t.inner_calls, 3);
}

#[test]
fn turn_groups_split_finished_turns_and_sum_usage() {
    let usage = SessionScriptUsage {
        turns: vec![
            turn(true, 2, 1, Some(10)),
            turn(true, 1, 2, None),
            turn(true, 3, 0, Some(5)),
            turn(true, 0, 0, Some(1)),
            turn(false, 4, 1, None),
        ],
        ..Default::default()
    };
    let mut builder = builder();
    builder.add_session("a", "s", usage);
    let t = builder.finish().totals;
    assert_eq!(t.rounds, 10);
    assert_eq!(t.unfinished_turns, 1);
    let g = &t.turns_with_scripts;
    assert_eq!((g.turns, g.rounds, g.turns_with_usage), (2, 3, 1));
    assert_eq!((g.usage.input_tokens, g.usage.output_tokens), (10, 20));
    let g = &t.turns_tools_only;
    assert_eq!((g.turns, g.rounds, g.turns_with_usage), (1, 3, 1));
    assert_eq!(g.usage.input_tokens, 5);
    let g = &t.turns_without_tools;
    assert_eq!((g.turns, g.rounds, g.turns_with_usage), (1, 0, 1));
}

#[test]
fn tools_sort_by_total_calls_then_name() {
    let mut usage = usage_of(vec![script(
        1,
        vec![row("b_tool", "ok", 3, 30), row("a_tool", "ok", 3, 6)],
        0,
    )]);
    usage.direct.insert("z_tool".into(), direct(5, 9));
    usage.direct.insert("a_tool".into(), direct(1, 4));
    let mut builder = builder();
    builder.add_session("a", "s", usage);
    let tools = builder.finish().tools;
    let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["z_tool", "a_tool", "b_tool"]);
    assert_eq!(tools[1].inner_calls, 3);
    assert_eq!(tools[1].direct_calls, 1);
    assert_eq!(tools[1].inner_result_bytes, 6);
    assert_eq!(tools[1].direct_result_bytes, 4);
}

#[test]
fn failures_are_newest_first() {
    let mut builder = builder();
    builder.add_session(
        "a",
        "s1",
        usage_of(vec![
            failed(7, Some(1), FailureKind::TimedOut),
            failed(3, None, FailureKind::Stopped),
        ]),
    );
    builder.add_session(
        "b",
        "s2",
        usage_of(vec![
            failed(9, Some(5), FailureKind::Exit127),
            failed(8, Some(1), FailureKind::Exit126),
        ]),
    );
    let report = builder.finish();
    let order: Vec<_> = report
        .failures
        .iter()
        .map(|f| (f.session_id.as_str(), f.seq))
        .collect();
    assert_eq!(order, [("s2", 9), ("s2", 8), ("s1", 7), ("s1", 3)]);
    assert_eq!(report.failures[0].agent, "b");
}

#[test]
fn failures_are_capped_and_counted() {
    let scripts = (0..205)
        .map(|i| failed(i, Some(1), FailureKind::NonZeroExit))
        .collect();
    let mut builder = builder();
    builder.add_session("a", "s", usage_of(scripts));
    let report = builder.finish();
    assert_eq!(report.failures.len(), FAILURE_LIST_LIMIT);
    assert_eq!(report.failures_omitted, 5);
    assert_eq!(report.failures[0].seq, 204);
    assert_eq!(
        report.totals.script_failures[&FailureKind::NonZeroExit],
        205
    );
}

#[test]
fn unreadable_sessions_are_counted() {
    let mut builder = builder();
    builder.add_unreadable();
    builder.add_unreadable();
    let report = builder.finish();
    assert_eq!(report.sessions_unreadable, 2);
    assert_eq!(report.sessions_scanned, 0);
}

#[test]
fn scope_is_echoed_in_the_report() {
    let scope = ScriptUsageScope {
        agent_pattern: Some("pantheon/*".into()),
        window: TimeWindow {
            since: Some(at(1)),
            until: Some(at(2)),
        },
    };
    let report = ReportBuilder::new("c", &scope).finish();
    assert_eq!(report.agent_pattern.as_deref(), Some("pantheon/*"));
    assert_eq!((report.since, report.until), (Some(at(1)), Some(at(2))));
}

#[test]
fn failure_kind_keys_serialize_as_snake_case_names() {
    let mut totals = UsageTotals::default();
    totals
        .script_failures
        .insert(FailureKind::InnerCallFailed, 1);
    let value = serde_json::to_value(&totals).unwrap();
    assert_eq!(value["script_failures"]["inner_call_failed"], 1);
}

#[test]
fn session_detail_carries_scripts_and_tools() {
    let mut run = failed(4, Some(1), FailureKind::NonZeroExit);
    run.failed_call = Some(FailedCall {
        name: "t".into(),
        status: "tool_error".into(),
    });
    let detail = session_detail("c", "a", "s", usage_of(vec![run]));
    assert_eq!(
        (detail.agent.as_str(), detail.session_id.as_str()),
        ("a", "s")
    );
    assert_eq!(detail.scripts.len(), 1);
    assert_eq!(detail.totals.scripts, 1);
}

#[test]
fn sums_of_stored_sizes_saturate_instead_of_overflowing() {
    let huge = || script(1, vec![row("fs_read", "ok", u64::MAX, u64::MAX)], u64::MAX);
    let usage = usage_of(vec![huge(), huge()]);
    let detail = session_detail("c", "a", "s", usage);
    assert_eq!(detail.totals.inner_result_bytes, u64::MAX);
    assert_eq!(detail.totals.inner_calls, u64::MAX);
    assert_eq!(detail.tools[0].inner_result_bytes, u64::MAX);
}

fn failed_with(seq: u64, kind: FailureKind, exit: Option<i64>, rows: Vec<TraceRow>) -> ScriptRun {
    ScriptRun {
        exit_code: exit,
        inner: rows,
        ..failed(seq, Some(1), kind)
    }
}

#[test]
fn plain_failures_are_counted_by_exit_code() {
    let usage = usage_of(vec![
        failed_with(2, FailureKind::NonZeroExit, Some(5), vec![]),
        failed_with(4, FailureKind::NonZeroExit, Some(5), vec![]),
        failed_with(6, FailureKind::NonZeroExit, Some(1), vec![]),
        failed_with(8, FailureKind::NonZeroExit, None, vec![]),
    ]);
    let mut builder = builder();
    builder.add_session("a", "s1", usage);
    let report = builder.finish();
    assert_eq!(
        report.totals.non_zero_exit_codes,
        std::collections::BTreeMap::from([(1, 1), (5, 2)])
    );
    assert_eq!(report.by_agent["a"].non_zero_exit_codes[&5], 2);
}

#[test]
fn other_failure_kinds_add_no_exit_code() {
    let usage = usage_of(vec![failed_with(
        2,
        FailureKind::InnerCallFailed,
        Some(1),
        vec![],
    )]);
    let mut builder = builder();
    builder.add_session("a", "s1", usage);
    assert!(builder.finish().totals.non_zero_exit_codes.is_empty());
}

#[test]
fn a_tool_counts_once_per_script_and_once_per_failed_script() {
    let usage = usage_of(vec![
        failed_with(
            2,
            FailureKind::NonZeroExit,
            Some(1),
            vec![
                row("fs_read", "ok", 2, 10),
                row("fs_read", "tool_error", 1, 10),
            ],
        ),
        script(4, vec![row("fs_read", "ok", 1, 10)], 0),
    ]);
    let mut builder = builder();
    builder.add_session("a", "s1", usage);
    let report = builder.finish();
    let tool = report.tools.iter().find(|t| t.name == "fs_read").unwrap();
    assert_eq!((tool.scripts, tool.failed_scripts), (2, 1));
    assert_eq!(tool.inner_calls, 4);
}

#[test]
fn a_failure_lists_its_tools_sorted_and_distinct() {
    let usage = usage_of(vec![failed_with(
        2,
        FailureKind::NonZeroExit,
        Some(1),
        vec![
            row("grep", "ok", 1, 1),
            row("fs_read", "ok", 1, 1),
            row("grep", "tool_error", 1, 1),
        ],
    )]);
    let mut builder = builder();
    builder.add_session("a", "s1", usage);
    let report = builder.finish();
    assert_eq!(report.failures[0].tools, vec!["fs_read", "grep"]);
}

#[test]
fn exit_code_keys_serialize_as_strings() {
    let mut totals = UsageTotals::default();
    totals.non_zero_exit_codes.insert(5, 3);
    let value = serde_json::to_value(&totals).unwrap();
    assert_eq!(value["non_zero_exit_codes"]["5"], 3);
}
