//! Text rendering for `harnx info script-usage`: turns the typed report into
//! compact, greppable operator output. Pure functions; the JSON path prints
//! the typed report itself and never comes through here.

use crate::script_usage_failures::{
    failed_tools_section, failures_section, kind_name, recent_failures_section,
};
use crate::script_usage_figures::{
    exit_code, kib, mean, num, percent, when, whole_mean, window_text, NONE,
};
use harnx_runtime::script_usage::{
    ScriptRun, ScriptUsageReport, SessionScriptUsageDetail, ToolShare, TurnGroup, UsageTotals,
};

const TOOL_LINES: usize = 20;

/// A titled block of two-space-indented lines; empty when there are no lines,
/// so a section with nothing to say is left out.
pub(crate) fn section(title: &str, lines: Vec<String>) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut out = format!("{title}\n");
    for line in lines {
        out.push_str(&format!("  {line}\n"));
    }
    out
}

fn trace_notes(totals: &UsageTotals) -> Vec<String> {
    let mut notes = Vec::new();
    if totals.trace_dropped > 0 {
        notes.push(format!(
            "note: {} inner calls fell beyond the retained-trace cap and are counted only in the buckets",
            num(totals.trace_dropped)
        ));
    }
    if totals.unparsed_trace_lines > 0 {
        notes.push(format!(
            "note: {} inner call trace lines could not be parsed",
            num(totals.unparsed_trace_lines)
        ));
    }
    notes
}

fn failed_scripts(totals: &UsageTotals) -> u64 {
    totals.scripts - totals.scripts_ok.min(totals.scripts)
}

fn tool_work_section(totals: &UsageTotals) -> String {
    let mut lines = vec![
        format!("model rounds: {}", num(totals.rounds)),
        format!(
            "direct calls: {} ({} errors)",
            num(totals.direct_calls),
            num(totals.direct_errors)
        ),
        format!(
            "scripts: {} (ok {}, failed {})",
            num(totals.scripts),
            num(totals.scripts_ok),
            num(failed_scripts(totals))
        ),
        format!(
            "inner calls: {} (mean {} per script, {} of all tool calls)",
            num(totals.inner_calls),
            mean(totals.inner_calls, totals.scripts),
            percent(
                totals.inner_calls,
                totals.inner_calls.saturating_add(totals.direct_calls)
            )
        ),
    ];
    lines.extend(trace_notes(totals));
    section("Tool work", lines)
}

fn buckets_section(totals: &UsageTotals) -> String {
    let buckets = &totals.calls_per_script;
    let lines = [
        ("0", buckets.zero),
        ("1", buckets.one),
        ("2-5", buckets.two_to_five),
        ("6-20", buckets.six_to_twenty),
        ("over 20", buckets.over_twenty),
    ]
    .into_iter()
    .map(|(label, count)| format!("{label}: {}", num(count)))
    .collect();
    section("Inner calls per script", lines)
}

fn bytes_section(totals: &UsageTotals) -> String {
    let mut lines = Vec::new();
    if totals.scripts > 0 {
        lines.extend([
            format!("script source: {}", kib(totals.script_bytes)),
            format!(
                "inner results (what scripts read): {}",
                kib(totals.inner_result_bytes)
            ),
            format!(
                "script results (what reached the model): {}",
                kib(totals.script_result_bytes)
            ),
        ]);
    }
    lines.push(format!(
        "direct results: {}",
        kib(totals.direct_result_bytes)
    ));
    section("Bytes", lines)
}

fn tools_section(tools: &[ToolShare]) -> String {
    let mut lines: Vec<String> = tools
        .iter()
        .take(TOOL_LINES)
        .map(|tool| {
            format!(
                "{}: {} / {}",
                tool.name,
                num(tool.direct_calls),
                num(tool.inner_calls)
            )
        })
        .collect();
    if tools.len() > TOOL_LINES {
        lines.push(format!(
            "... {} more in --json",
            num((tools.len() - TOOL_LINES) as u64)
        ));
    }
    section("Tools (direct / in scripts)", lines)
}

fn turn_row(label: &str, group: &TurnGroup) -> String {
    format!(
        "{label}: {} turns, {} rounds per turn, {} input / {} output tokens per turn",
        num(group.turns),
        mean(group.rounds, group.turns),
        whole_mean(group.usage.input_tokens, group.turns_with_usage),
        whole_mean(group.usage.output_tokens, group.turns_with_usage)
    )
}

fn turns_section(totals: &UsageTotals) -> String {
    let groups = [
        &totals.turns_with_scripts,
        &totals.turns_tools_only,
        &totals.turns_without_tools,
    ];
    if totals.unfinished_turns == 0 && groups.iter().all(|group| group.turns == 0) {
        return String::new();
    }
    section(
        "Turns (finished)",
        vec![
            turn_row("with scripts", &totals.turns_with_scripts),
            turn_row("tools only", &totals.turns_tools_only),
            turn_row("without tools", &totals.turns_without_tools),
            format!("unfinished turns: {}", num(totals.unfinished_turns)),
        ],
    )
}

fn agents_section(report: &ScriptUsageReport) -> String {
    if report.by_agent.len() < 2 {
        return String::new();
    }
    let mut agents: Vec<_> = report.by_agent.iter().collect();
    agents.sort_by(|a, b| b.1.scripts.cmp(&a.1.scripts).then_with(|| a.0.cmp(b.0)));
    let lines = agents
        .into_iter()
        .map(|(name, totals)| {
            format!(
                "{name}: {} scripts, {} failed, {} inner calls, {} direct calls",
                num(totals.scripts),
                num(failed_scripts(totals)),
                num(totals.inner_calls),
                num(totals.direct_calls)
            )
        })
        .collect();
    section("By agent", lines)
}

fn header_section(report: &ScriptUsageReport) -> String {
    format!(
        "Script usage on cluster '{}'\nagents: {}\nwindow: {}\n",
        report.cluster,
        report.agent_pattern.as_deref().unwrap_or("all agents"),
        window_text(report.since, report.until)
    )
}

fn sessions_section(report: &ScriptUsageReport) -> String {
    section(
        "Sessions",
        vec![
            format!("scanned: {}", num(report.sessions_scanned)),
            format!("with scripts: {}", num(report.sessions_with_scripts)),
            format!("unreadable: {}", num(report.sessions_unreadable)),
        ],
    )
}

fn join_sections(sections: Vec<String>) -> String {
    sections
        .into_iter()
        .filter(|section| !section.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn render_report_text(report: &ScriptUsageReport) -> String {
    let totals = &report.totals;
    let mut sections = vec![
        header_section(report),
        sessions_section(report),
        tool_work_section(totals),
    ];
    if totals.scripts == 0 {
        sections.push("No bash scripts in scope.\n".to_string());
    } else {
        sections.extend([
            buckets_section(totals),
            failures_section(totals),
            failed_tools_section(&report.tools),
        ]);
    }
    sections.extend([
        bytes_section(totals),
        tools_section(&report.tools),
        turns_section(totals),
        agents_section(report),
    ]);
    if totals.scripts > 0 {
        sections.push(recent_failures_section(report));
    }
    join_sections(sections)
}

fn script_line(script: &ScriptRun) -> String {
    let inner = script
        .inner
        .iter()
        .fold(0u64, |sum, row| sum.saturating_add(row.count));
    let failure = script.failure.map(kind_name);
    format!(
        "seq {} {} {}: exit {}, failure {}, {} inner calls, script {} B, result {} B",
        script.seq,
        when(script.timestamp),
        script.status,
        exit_code(script.exit_code),
        failure.as_deref().unwrap_or(NONE),
        num(inner.saturating_add(script.trace_dropped)),
        num(script.script_bytes),
        num(script.result_bytes)
    )
}

fn scripts_section(scripts: &[ScriptRun]) -> String {
    if scripts.is_empty() {
        return "No bash scripts in this session.\n".to_string();
    }
    let mut ordered: Vec<&ScriptRun> = scripts.iter().collect();
    ordered.sort_by_key(|script| script.seq);
    section("Scripts", ordered.into_iter().map(script_line).collect())
}

pub fn render_session_text(detail: &SessionScriptUsageDetail) -> String {
    join_sections(vec![
        format!(
            "Script usage for session '{}' (agent '{}') on cluster '{}'\n",
            detail.session_id, detail.agent, detail.cluster
        ),
        tool_work_section(&detail.totals),
        bytes_section(&detail.totals),
        tools_section(&detail.tools),
        scripts_section(&detail.scripts),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use harnx_core::api_types::CompletionTokenUsage;
    use harnx_runtime::script_usage::{
        CallsPerScript, FailedCall, FailureKind, FailureRef, ScriptUsageReport,
    };
    use std::collections::BTreeMap;

    fn time(minutes: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + minutes * 60, 0)
            .single()
            .expect("time")
    }

    fn full_totals() -> UsageTotals {
        UsageTotals {
            rounds: 1_200,
            direct_calls: 600,
            direct_errors: 12,
            direct_result_bytes: 307_200,
            scripts: 40,
            scripts_ok: 30,
            script_failures: BTreeMap::from([
                (FailureKind::Exit127, 3),
                (FailureKind::Exit126, 1),
                (FailureKind::NonZeroExit, 6),
            ]),
            inner_calls: 1_800,
            trace_dropped: 5,
            unparsed_trace_lines: 2,
            calls_per_script: CallsPerScript {
                zero: 1,
                one: 4,
                two_to_five: 20,
                six_to_twenty: 10,
                over_twenty: 5,
            },
            script_bytes: 20_480,
            inner_result_bytes: 5_242_880,
            script_result_bytes: 102_400,
            unfinished_turns: 3,
            turns_with_scripts: TurnGroup {
                turns: 20,
                rounds: 90,
                turns_with_usage: 10,
                usage: CompletionTokenUsage {
                    input_tokens: 1_500_000,
                    output_tokens: 25_000,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
            },
            turns_tools_only: TurnGroup {
                turns: 5,
                rounds: 7,
                ..TurnGroup::default()
            },
            ..UsageTotals::default()
        }
    }

    fn agent_totals(scripts: u64, ok: u64) -> UsageTotals {
        UsageTotals {
            scripts,
            scripts_ok: ok,
            inner_calls: scripts * 3,
            direct_calls: 10,
            ..UsageTotals::default()
        }
    }

    fn failure(seq: u64) -> FailureRef {
        FailureRef {
            agent: "pantheon/atlas".to_string(),
            session_id: "abc123".to_string(),
            seq,
            timestamp: Some(time(seq as i64)),
            kind: FailureKind::Exit127,
            exit_code: Some(127),
            failed_call: Some(FailedCall {
                name: "fs_read".to_string(),
                status: "tool_error".to_string(),
            }),
            tools: Vec::new(),
        }
    }

    fn tool(name: &str, direct: u64, inner: u64) -> ToolShare {
        ToolShare {
            name: name.to_string(),
            direct_calls: direct,
            inner_calls: inner,
            direct_result_bytes: 0,
            inner_result_bytes: 0,
            scripts: 0,
            failed_scripts: 0,
        }
    }

    fn failing_tool(name: &str, scripts: u64, failed_scripts: u64) -> ToolShare {
        ToolShare {
            scripts,
            failed_scripts,
            ..tool(name, 0, 1)
        }
    }

    fn full_report() -> ScriptUsageReport {
        ScriptUsageReport {
            cluster: "prod".to_string(),
            agent_pattern: Some("pantheon*".to_string()),
            since: Some(time(0)),
            until: None,
            sessions_scanned: 1_234,
            sessions_with_scripts: 56,
            sessions_unreadable: 2,
            totals: full_totals(),
            by_agent: BTreeMap::from([
                ("pantheon/atlas".to_string(), agent_totals(30, 25)),
                ("(inline)".to_string(), agent_totals(10, 5)),
            ]),
            tools: vec![
                ToolShare {
                    scripts: 30,
                    failed_scripts: 10,
                    ..tool("fs_read", 400, 900)
                },
                tool("bash", 200, 0),
            ],
            failures: (1..=12).map(failure).collect(),
            failures_omitted: 3,
        }
    }

    fn empty_report() -> ScriptUsageReport {
        ScriptUsageReport {
            cluster: "__local__".to_string(),
            agent_pattern: None,
            since: None,
            until: None,
            sessions_scanned: 0,
            sessions_with_scripts: 0,
            sessions_unreadable: 0,
            totals: UsageTotals::default(),
            by_agent: BTreeMap::new(),
            tools: Vec::new(),
            failures: Vec::new(),
            failures_omitted: 0,
        }
    }

    #[test]
    fn full_report_text_covers_every_section_in_order() {
        let out = render_report_text(&full_report());
        let headings = [
            "Script usage on cluster 'prod'",
            "Sessions",
            "Tool work",
            "Inner calls per script",
            "Script failures",
            "Tools in failed scripts",
            "Bytes",
            "Tools (direct / in scripts)",
            "Turns (finished)",
            "By agent",
            "Recent failures",
        ];
        let mut at = 0;
        for heading in headings {
            let found = out[at..]
                .find(heading)
                .unwrap_or_else(|| panic!("missing or out of order: {heading}\n{out}"));
            at += found + heading.len();
        }
        assert!(out.contains("agents: pantheon*"), "{out}");
        assert!(out.contains("scanned: 1,234"), "{out}");
        assert!(out.contains("scripts: 40 (ok 30, failed 10)"), "{out}");
        assert!(
            out.contains("inner calls: 1,800 (mean 45.0 per script, 75% of all tool calls)"),
            "{out}"
        );
        assert!(out.contains("5 inner calls fell beyond"), "{out}");
        assert!(
            out.contains("2 inner call trace lines could not be parsed"),
            "{out}"
        );
        assert!(out.contains("script source: 20.0 KiB"), "{out}");
        assert!(
            out.contains("inner results (what scripts read): 5,120.0 KiB"),
            "{out}"
        );
        assert!(
            out.contains("script results (what reached the model): 100.0 KiB"),
            "{out}"
        );
        assert!(out.contains("direct results: 300.0 KiB"), "{out}");
        assert!(out.contains("fs_read: 400 / 900"), "{out}");
        assert!(
            out.contains("with scripts: 20 turns, 4.5 rounds per turn, 150,000 input / 2,500 output tokens per turn"),
            "{out}"
        );
        assert!(out.contains("unfinished turns: 3"), "{out}");
        assert!(
            out.contains("pantheon/atlas: 30 scripts, 5 failed, 90 inner calls, 10 direct calls"),
            "{out}"
        );
        assert!(
            out.contains("pantheon/atlas abc123 seq 1 2027-01-15T08:01:00Z: exit 127, exit_127, failed call fs_read (tool_error)"),
            "{out}"
        );
        assert!(out.contains("... 5 more; --json lists up to 200"), "{out}");
    }

    #[test]
    fn failures_list_most_frequent_first_with_readable_exit_labels() {
        let out = render_report_text(&full_report());
        let non_zero = out.find("non_zero_exit: 6").expect("non_zero_exit");
        let not_found = out
            .find("exit 127 (command not found): 3")
            .expect("exit 127 label");
        let cannot_run = out
            .find("exit 126 (an inner call needed approval, was blocked or was rejected): 1")
            .expect("exit 126 label");
        assert!(non_zero < not_found && not_found < cannot_run, "{out}");
    }

    fn failure_line_report(codes: &[(i64, u64)]) -> String {
        let mut report = full_report();
        report.totals.script_failures = BTreeMap::from([(
            FailureKind::NonZeroExit,
            codes.iter().map(|(_, count)| count).sum(),
        )]);
        report.totals.non_zero_exit_codes = codes.iter().copied().collect();
        render_report_text(&report)
    }

    #[test]
    fn the_plain_failure_line_lists_its_exit_codes() {
        let out = failure_line_report(&[(1, 200), (2, 60), (5, 300)]);
        assert!(
            out.contains("non_zero_exit: 560 (exit 5: 300, exit 1: 200, exit 2: 60)\n"),
            "{out}"
        );
        let codes: Vec<(i64, u64)> = (1..=7).map(|code| (code, 100 - code as u64)).collect();
        let out = failure_line_report(&codes);
        assert!(
            out.contains(
                "non_zero_exit: 672 (exit 1: 99, exit 2: 98, exit 3: 97, exit 4: 96, exit 5: 95, other: 187)\n"
            ),
            "{out}"
        );
        let out = render_report_text(&full_report());
        assert!(out.contains("non_zero_exit: 6\n"), "{out}");
    }

    #[test]
    fn tools_in_failed_scripts_are_listed_by_failures() {
        let mut report = full_report();
        report.tools = vec![
            failing_tool("zeta", 3, 2),
            failing_tool("fs_read", 300, 100),
            failing_tool("alpha", 4, 2),
            failing_tool("clean", 10, 0),
        ];
        let out = render_report_text(&report);
        let section = out
            .split("Tools in failed scripts\n")
            .nth(1)
            .expect("section")
            .split("\n\n")
            .next()
            .expect("block");
        assert_eq!(
            section,
            "  fs_read: 100 of 300 scripts failed (33%)\n  alpha: 2 of 4 scripts failed (50%)\n  zeta: 2 of 3 scripts failed (67%)"
        );
        assert!(!section.contains("clean:"), "{out}");

        report.tools = (0..11)
            .map(|n| failing_tool(&format!("tool_{n:02}"), 10, 1))
            .collect();
        let out = render_report_text(&report);
        let section = out
            .split("Tools in failed scripts\n")
            .nth(1)
            .expect("section")
            .split("\n\n")
            .next()
            .expect("block");
        assert_eq!(section.matches("scripts failed").count(), 10, "{out}");
        assert!(section.ends_with("  ... 1 more in --json"), "{out}");
        assert!(!section.contains("tool_10"), "{out}");
    }

    #[test]
    fn a_recent_failure_names_its_tools() {
        let mut report = full_report();
        report.failures[0].tools = vec!["fs_read".to_string(), "grep".to_string()];
        report.failures[1].tools = ["a", "b", "c", "d", "e", "f"].map(String::from).to_vec();
        let out = render_report_text(&report);
        assert!(
            out.contains("failed call fs_read (tool_error), tools: fs_read, grep\n"),
            "{out}"
        );
        assert!(
            out.contains("(tool_error), tools: a, b, c, d, e, ...\n"),
            "{out}"
        );
        assert_eq!(out.matches(", tools:").count(), 2, "{out}");
    }

    #[test]
    fn recent_failures_stop_at_ten_lines() {
        let out = render_report_text(&full_report());
        let section = out.split("Recent failures\n").nth(1).expect("section");
        assert_eq!(section.matches("seq ").count(), 10, "{out}");
    }

    #[test]
    fn single_agent_report_omits_the_agent_split() {
        let mut report = full_report();
        report.by_agent.remove("(inline)");
        assert!(!render_report_text(&report).contains("By agent"));
    }

    #[test]
    fn empty_report_says_there_are_no_scripts() {
        let out = render_report_text(&empty_report());
        assert!(out.contains("agents: all agents"), "{out}");
        assert!(out.contains("window: all time"), "{out}");
        assert!(out.contains("Sessions"), "{out}");
        assert!(out.contains("Tool work"), "{out}");
        assert!(out.contains("No bash scripts in scope."), "{out}");
        assert!(
            out.contains("(mean - per script, - of all tool calls)"),
            "{out}"
        );
        assert!(out.contains("direct results: 0.0 KiB"), "{out}");
        for absent in [
            "Turns (finished)",
            "Script failures",
            "Tools in failed scripts",
            "Recent failures",
            "script source",
        ] {
            assert!(!out.contains(absent), "{absent} in {out}");
        }
    }

    #[test]
    fn a_report_without_scripts_still_shows_tools_and_turns() {
        let mut report = empty_report();
        report.totals.rounds = 12;
        report.totals.direct_calls = 10;
        report.totals.direct_result_bytes = 2048;
        report.totals.turns_tools_only = TurnGroup {
            turns: 4,
            rounds: 12,
            turns_with_usage: 2,
            usage: CompletionTokenUsage {
                input_tokens: 2_000,
                output_tokens: 100,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
        };
        report.tools = vec![tool("fs_read", 10, 0)];
        let out = render_report_text(&report);
        assert!(out.contains("No bash scripts in scope."), "{out}");
        assert!(out.contains("Tools (direct / in scripts)"), "{out}");
        assert!(out.contains("fs_read: 10 / 0"), "{out}");
        assert!(out.contains("Turns (finished)"), "{out}");
        assert!(
            out.contains("tools only: 4 turns, 3.0 rounds per turn, 1,000 input / 50 output"),
            "{out}"
        );
        assert!(out.contains("direct results: 2.0 KiB"), "{out}");
        for absent in [
            "Inner calls per script",
            "Script failures",
            "Tools in failed scripts",
            "Recent failures",
            "script source",
        ] {
            assert!(!out.contains(absent), "{absent} in {out}");
        }
    }

    #[test]
    fn zero_denominator_turn_rows_print_a_dash() {
        let out = render_report_text(&full_report());
        assert!(
            out.contains(
                "tools only: 5 turns, 1.4 rounds per turn, - input / - output tokens per turn"
            ),
            "{out}"
        );
        assert!(
            out.contains(
                "without tools: 0 turns, - rounds per turn, - input / - output tokens per turn"
            ),
            "{out}"
        );
    }

    fn script(seq: u64, failure: Option<FailureKind>) -> ScriptRun {
        ScriptRun {
            seq,
            timestamp: Some(time(seq as i64)),
            status: if failure.is_some() { "error" } else { "ok" }.to_string(),
            exit_code: Some(if failure.is_some() { 127 } else { 0 }),
            failure,
            failed_call: None,
            inner: vec![harnx_runtime::script_usage::TraceRow {
                name: "fs_read".to_string(),
                status: "ok".to_string(),
                count: 3,
                bytes: 100,
                duration_ms: 5,
            }],
            trace_dropped: 1,
            unparsed_trace_lines: 0,
            script_bytes: 2_048,
            result_bytes: 4_096,
            output_truncated: false,
        }
    }

    fn session_detail() -> SessionScriptUsageDetail {
        SessionScriptUsageDetail {
            cluster: "prod".to_string(),
            agent: "pantheon/atlas".to_string(),
            session_id: "abc123".to_string(),
            totals: full_totals(),
            tools: vec![tool("fs_read", 4, 9)],
            scripts: vec![script(9, Some(FailureKind::Exit127)), script(3, None)],
        }
    }

    #[test]
    fn session_text_lists_scripts_in_seq_order_after_the_shared_sections() {
        let out = render_session_text(&session_detail());
        assert!(
            out.contains(
                "Script usage for session 'abc123' (agent 'pantheon/atlas') on cluster 'prod'"
            ),
            "{out}"
        );
        let work = out.find("Tool work").expect("tool work");
        let bytes = out.find("Bytes").expect("bytes");
        let tools = out.find("Tools (direct / in scripts)").expect("tools");
        let scripts = out.find("Scripts\n").expect("scripts");
        assert!(work < bytes && bytes < tools && tools < scripts, "{out}");
        let first = out.find("seq 3 ").expect("seq 3");
        let second = out.find("seq 9 ").expect("seq 9");
        assert!(first < second, "{out}");
        assert!(
            out.contains(
                "exit 127, failure exit_127, 4 inner calls, script 2,048 B, result 4,096 B"
            ),
            "{out}"
        );
        assert!(out.contains("exit 0, failure -, 4 inner calls"), "{out}");
    }

    #[test]
    fn session_text_without_scripts_says_so() {
        let mut detail = session_detail();
        detail.scripts.clear();
        let out = render_session_text(&detail);
        assert!(out.contains("No bash scripts in this session."), "{out}");
    }
}
