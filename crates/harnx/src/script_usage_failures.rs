//! The failure parts of the script usage text report: kind names, the failure
//! counts and the recent failure list.

use crate::script_usage_figures::{exit_code, num, percent, when};
use crate::script_usage_render::section;
use harnx_runtime::script_usage::{
    FailureKind, FailureRef, ScriptUsageReport, ToolShare, UsageTotals, FAILURE_LIST_LIMIT,
};

const FAILURE_LINES: usize = 10;
const EXIT_CODES_SHOWN: usize = 5;
const FAILED_TOOL_LINES: usize = 10;
const FAILURE_TOOLS_SHOWN: usize = 5;

pub(crate) fn kind_name(kind: FailureKind) -> String {
    kind.as_str().to_string()
}

fn kind_label(kind: FailureKind) -> String {
    match kind {
        FailureKind::Exit126 => {
            "exit 126 (an inner call needed approval, was blocked or was rejected)".to_string()
        }
        FailureKind::Exit127 => "exit 127 (command not found)".to_string(),
        other => kind_name(other),
    }
}

/// The exit codes behind `non_zero_exit`, most frequent first; empty when no
/// code is known.
fn exit_codes_suffix(totals: &UsageTotals) -> String {
    let mut codes: Vec<_> = totals.non_zero_exit_codes.iter().collect();
    codes.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let mut parts: Vec<String> = codes
        .iter()
        .take(EXIT_CODES_SHOWN)
        .map(|(code, count)| format!("exit {code}: {}", num(**count)))
        .collect();
    let rest = codes
        .iter()
        .skip(EXIT_CODES_SHOWN)
        .fold(0u64, |sum, (_, count)| sum.saturating_add(**count));
    if codes.len() > EXIT_CODES_SHOWN {
        parts.push(format!("other: {}", num(rest)));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

pub(crate) fn failures_section(totals: &UsageTotals) -> String {
    let mut kinds: Vec<_> = totals.script_failures.iter().collect();
    kinds.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let lines = kinds
        .into_iter()
        .map(|(kind, count)| {
            let codes = if *kind == FailureKind::NonZeroExit {
                exit_codes_suffix(totals)
            } else {
                String::new()
            };
            format!("{}: {}{codes}", kind_label(*kind), num(*count))
        })
        .collect();
    section("Script failures", lines)
}

/// Tools called by failed scripts. A tool listed here was in a script that
/// failed, which does not make it the cause.
pub(crate) fn failed_tools_section(tools: &[ToolShare]) -> String {
    let mut failing: Vec<&ToolShare> = tools.iter().filter(|t| t.failed_scripts > 0).collect();
    failing.sort_by(|a, b| {
        b.failed_scripts
            .cmp(&a.failed_scripts)
            .then_with(|| a.name.cmp(&b.name))
    });
    let mut lines: Vec<String> = failing
        .iter()
        .take(FAILED_TOOL_LINES)
        .map(|tool| {
            format!(
                "{}: {} of {} scripts failed ({})",
                tool.name,
                num(tool.failed_scripts),
                num(tool.scripts),
                percent(tool.failed_scripts, tool.scripts)
            )
        })
        .collect();
    if failing.len() > FAILED_TOOL_LINES {
        lines.push(format!(
            "... {} more in --json",
            num((failing.len() - FAILED_TOOL_LINES) as u64)
        ));
    }
    section("Tools in failed scripts", lines)
}

fn failure_line(failure: &FailureRef) -> String {
    let call = failure
        .failed_call
        .as_ref()
        .map(|call| format!(", failed call {} ({})", call.name, call.status))
        .unwrap_or_default();
    let tools = match failure.tools.len() {
        0 => String::new(),
        n if n > FAILURE_TOOLS_SHOWN => format!(
            ", tools: {}, ...",
            failure.tools[..FAILURE_TOOLS_SHOWN].join(", ")
        ),
        _ => format!(", tools: {}", failure.tools.join(", ")),
    };
    format!(
        "{} {} seq {} {}: exit {}, {}{call}{tools}",
        failure.agent,
        failure.session_id,
        failure.seq,
        when(failure.timestamp),
        exit_code(failure.exit_code),
        kind_name(failure.kind)
    )
}

pub(crate) fn recent_failures_section(report: &ScriptUsageReport) -> String {
    let mut lines: Vec<String> = report
        .failures
        .iter()
        .take(FAILURE_LINES)
        .map(failure_line)
        .collect();
    let listed = lines.len() as u64;
    let total = report.failures.len() as u64 + report.failures_omitted;
    if total > listed {
        lines.push(format!(
            "... {} more; --json lists up to {} and counts the rest",
            num(total - listed),
            num(FAILURE_LIST_LIMIT as u64)
        ));
    }
    section("Recent failures", lines)
}
