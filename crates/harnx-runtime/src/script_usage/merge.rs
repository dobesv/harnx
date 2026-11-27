//! Merging per-session facts into report totals and per-tool counters.

use std::collections::{BTreeMap, BTreeSet};

use super::report::{CallsPerScript, ToolShare, TurnGroup, UsageTotals};
use super::session::{FailureKind, ScriptRun, SessionScriptUsage, TurnFacts};

/// Per-tool counters while sessions merge.
pub(super) type ToolMap = BTreeMap<String, ToolShare>;

pub(super) fn tool_entry<'a>(tools: &'a mut ToolMap, name: &str) -> &'a mut ToolShare {
    tools.entry(name.to_string()).or_insert_with(|| ToolShare {
        name: name.to_string(),
        direct_calls: 0,
        inner_calls: 0,
        direct_result_bytes: 0,
        inner_result_bytes: 0,
        scripts: 0,
        failed_scripts: 0,
    })
}

/// Distinct inner tool names of a script, sorted.
pub(super) fn script_tools(script: &ScriptRun) -> BTreeSet<&str> {
    script.inner.iter().map(|row| row.name.as_str()).collect()
}

fn add_group(group: &mut TurnGroup, turn: &TurnFacts) {
    group.turns += 1;
    add(&mut group.rounds, turn.rounds);
    if let Some(usage) = &turn.usage {
        group.turns_with_usage += 1;
        group.usage.accumulate(usage);
    }
}

fn add_turn(totals: &mut UsageTotals, turn: &TurnFacts) {
    add(&mut totals.rounds, turn.rounds);
    if !turn.finished {
        totals.unfinished_turns += 1;
        return;
    }
    let group = if turn.scripts > 0 {
        &mut totals.turns_with_scripts
    } else if turn.rounds > 0 {
        &mut totals.turns_tools_only
    } else {
        &mut totals.turns_without_tools
    };
    add_group(group, turn);
}

/// Stored sizes and counts come from parsed text, so sums must not overflow.
fn add(slot: &mut u64, value: u64) {
    *slot = slot.saturating_add(value);
}

const TWO_TO_FIVE_MAX: u64 = 5;
const SIX_TO_TWENTY_MAX: u64 = 20;

fn add_bucket(buckets: &mut CallsPerScript, calls: u64) {
    let slot = if calls == 0 {
        &mut buckets.zero
    } else if calls == 1 {
        &mut buckets.one
    } else if calls <= TWO_TO_FIVE_MAX {
        &mut buckets.two_to_five
    } else if calls <= SIX_TO_TWENTY_MAX {
        &mut buckets.six_to_twenty
    } else {
        &mut buckets.over_twenty
    };
    *slot += 1;
}

fn add_script(totals: &mut UsageTotals, tools: &mut ToolMap, script: &ScriptRun) {
    totals.scripts += 1;
    match script.failure {
        None => totals.scripts_ok += 1,
        Some(kind) => add(totals.script_failures.entry(kind).or_default(), 1),
    }
    if let (Some(FailureKind::NonZeroExit), Some(code)) = (script.failure, script.exit_code) {
        add(totals.non_zero_exit_codes.entry(code).or_default(), 1);
    }
    add(&mut totals.trace_dropped, script.trace_dropped);
    add(
        &mut totals.unparsed_trace_lines,
        script.unparsed_trace_lines,
    );
    add(&mut totals.script_bytes, script.script_bytes);
    add(&mut totals.script_result_bytes, script.result_bytes);
    let mut row_calls: u64 = 0;
    for row in &script.inner {
        add(&mut row_calls, row.count);
        let status = totals.inner_calls_by_status.entry(row.status.clone());
        add(status.or_default(), row.count);
        add(&mut totals.inner_result_bytes, row.bytes);
        let tool = tool_entry(tools, &row.name);
        add(&mut tool.inner_calls, row.count);
        add(&mut tool.inner_result_bytes, row.bytes);
    }
    add(&mut totals.inner_calls, row_calls);
    for name in script_tools(script) {
        let tool = tool_entry(tools, name);
        add(&mut tool.scripts, 1);
        if script.failure.is_some() {
            add(&mut tool.failed_scripts, 1);
        }
    }
    let all_calls = row_calls.saturating_add(script.trace_dropped);
    add_bucket(&mut totals.calls_per_script, all_calls);
}

fn add_direct(totals: &mut UsageTotals, tools: &mut ToolMap, usage: &SessionScriptUsage) {
    for (name, stats) in &usage.direct {
        add(&mut totals.direct_calls, stats.calls);
        add(&mut totals.direct_errors, stats.errors);
        add(&mut totals.direct_args_bytes, stats.args_bytes);
        add(&mut totals.direct_result_bytes, stats.result_bytes);
        let tool = tool_entry(tools, name);
        add(&mut tool.direct_calls, stats.calls);
        add(&mut tool.direct_result_bytes, stats.result_bytes);
    }
}

pub(super) fn merge_session(
    totals: &mut UsageTotals,
    tools: &mut ToolMap,
    usage: &SessionScriptUsage,
) {
    add_direct(totals, tools, usage);
    for script in &usage.scripts {
        add_script(totals, tools, script);
    }
    for turn in &usage.turns {
        add_turn(totals, turn);
    }
}

pub(super) fn sorted_tools(tools: ToolMap) -> Vec<ToolShare> {
    let mut list: Vec<ToolShare> = tools.into_values().collect();
    list.sort_by(|a, b| {
        b.direct_calls
            .saturating_add(b.inner_calls)
            .cmp(&a.direct_calls.saturating_add(a.inner_calls))
            .then_with(|| a.name.cmp(&b.name))
    });
    list
}
