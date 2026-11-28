//! The script usage report: per-session facts merged into totals, a per-agent
//! split, a per-tool split and a list of failed scripts. Holds counts, names,
//! ids and sizes only.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use harnx_core::api_types::CompletionTokenUsage;
use serde::Serialize;

use super::merge::{merge_session, script_tools, sorted_tools, ToolMap};
use super::session::{FailedCall, FailureKind, ScriptRun, SessionScriptUsage, TimeWindow};

pub const FAILURE_LIST_LIMIT: usize = 200;
pub const INLINE_AGENT_LABEL: &str = "(inline)";

#[derive(Debug, Clone, Default, Serialize)]
pub struct TurnGroup {
    pub turns: u64,
    pub rounds: u64,
    pub turns_with_usage: u64,
    pub usage: CompletionTokenUsage,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CallsPerScript {
    pub zero: u64,
    pub one: u64,
    pub two_to_five: u64,
    pub six_to_twenty: u64,
    pub over_twenty: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageTotals {
    pub rounds: u64,
    pub direct_calls: u64,
    pub direct_errors: u64,
    pub direct_args_bytes: u64,
    pub direct_result_bytes: u64,
    pub scripts: u64,
    pub scripts_ok: u64,
    pub script_failures: BTreeMap<FailureKind, u64>,
    /// Plain failures (`non_zero_exit`) by exit code; the other kinds already
    /// say what happened.
    pub non_zero_exit_codes: BTreeMap<i64, u64>,
    pub inner_calls: u64,
    pub inner_calls_by_status: BTreeMap<String, u64>,
    pub trace_dropped: u64,
    pub unparsed_trace_lines: u64,
    pub calls_per_script: CallsPerScript,
    pub script_bytes: u64,
    pub script_result_bytes: u64,
    pub inner_result_bytes: u64,
    pub unfinished_turns: u64,
    /// Finished turns with at least one script.
    pub turns_with_scripts: TurnGroup,
    /// Finished turns with rounds and no script.
    pub turns_tools_only: TurnGroup,
    /// Finished turns with no round.
    pub turns_without_tools: TurnGroup,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolShare {
    pub name: String,
    pub direct_calls: u64,
    pub inner_calls: u64,
    pub direct_result_bytes: u64,
    pub inner_result_bytes: u64,
    /// Scripts that called the tool at least once.
    pub scripts: u64,
    /// Those of them that failed. Being in a failed script does not make the
    /// tool the cause.
    pub failed_scripts: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailureRef {
    pub agent: String,
    pub session_id: String,
    pub seq: u64,
    pub timestamp: Option<DateTime<Utc>>,
    pub kind: FailureKind,
    pub exit_code: Option<i64>,
    pub failed_call: Option<FailedCall>,
    /// Distinct inner tool names of the script, sorted.
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScriptUsageReport {
    pub cluster: String,
    pub agent_pattern: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub sessions_scanned: u64,
    pub sessions_with_scripts: u64,
    pub sessions_unreadable: u64,
    pub totals: UsageTotals,
    pub by_agent: BTreeMap<String, UsageTotals>,
    pub tools: Vec<ToolShare>,
    pub failures: Vec<FailureRef>,
    pub failures_omitted: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionScriptUsageDetail {
    pub cluster: String,
    pub agent: String,
    pub session_id: String,
    pub totals: UsageTotals,
    pub tools: Vec<ToolShare>,
    pub scripts: Vec<ScriptRun>,
}

#[derive(Debug, Clone, Default)]
pub struct ScriptUsageScope {
    pub agent_pattern: Option<String>,
    pub window: TimeWindow,
}

/// One session's totals, tools and scripts, for the session view.
pub fn session_detail(
    cluster: &str,
    agent: &str,
    session_id: &str,
    usage: SessionScriptUsage,
) -> SessionScriptUsageDetail {
    let mut totals = UsageTotals::default();
    let mut tools = ToolMap::new();
    merge_session(&mut totals, &mut tools, &usage);
    SessionScriptUsageDetail {
        cluster: cluster.to_string(),
        agent: agent.to_string(),
        session_id: session_id.to_string(),
        totals,
        tools: sorted_tools(tools),
        scripts: usage.scripts,
    }
}

pub struct ReportBuilder {
    cluster: String,
    scope: ScriptUsageScope,
    sessions_scanned: u64,
    sessions_with_scripts: u64,
    sessions_unreadable: u64,
    totals: UsageTotals,
    by_agent: BTreeMap<String, UsageTotals>,
    tools: ToolMap,
    failures: Vec<FailureRef>,
}

impl ReportBuilder {
    pub fn new(cluster: &str, scope: &ScriptUsageScope) -> Self {
        Self {
            cluster: cluster.to_string(),
            scope: scope.clone(),
            sessions_scanned: 0,
            sessions_with_scripts: 0,
            sessions_unreadable: 0,
            totals: UsageTotals::default(),
            by_agent: BTreeMap::new(),
            tools: ToolMap::new(),
            failures: Vec::new(),
        }
    }

    pub fn add_session(&mut self, agent: &str, session_id: &str, usage: SessionScriptUsage) {
        self.sessions_scanned += 1;
        if !usage.scripts.is_empty() {
            self.sessions_with_scripts += 1;
        }
        merge_session(&mut self.totals, &mut self.tools, &usage);
        // The per-agent split has no tool list of its own.
        let mut ignored = ToolMap::new();
        let agent_totals = self.by_agent.entry(agent.to_string()).or_default();
        merge_session(agent_totals, &mut ignored, &usage);
        self.failures
            .extend(failure_refs(agent, session_id, &usage));
    }

    pub fn add_unreadable(&mut self) {
        self.sessions_unreadable += 1;
    }

    pub fn finish(mut self) -> ScriptUsageReport {
        self.failures.sort_by(|a, b| {
            (b.timestamp, b.seq, &b.session_id).cmp(&(a.timestamp, a.seq, &a.session_id))
        });
        let omitted = self.failures.len().saturating_sub(FAILURE_LIST_LIMIT);
        self.failures.truncate(FAILURE_LIST_LIMIT);
        ScriptUsageReport {
            cluster: self.cluster,
            agent_pattern: self.scope.agent_pattern,
            since: self.scope.window.since,
            until: self.scope.window.until,
            sessions_scanned: self.sessions_scanned,
            sessions_with_scripts: self.sessions_with_scripts,
            sessions_unreadable: self.sessions_unreadable,
            totals: self.totals,
            by_agent: self.by_agent,
            tools: sorted_tools(self.tools),
            failures: self.failures,
            failures_omitted: omitted as u64,
        }
    }
}

fn failure_refs<'a>(
    agent: &'a str,
    session_id: &'a str,
    usage: &'a SessionScriptUsage,
) -> impl Iterator<Item = FailureRef> + 'a {
    usage.scripts.iter().filter_map(move |script| {
        Some(FailureRef {
            agent: agent.to_string(),
            session_id: session_id.to_string(),
            seq: script.seq,
            timestamp: script.timestamp,
            kind: script.failure?,
            exit_code: script.exit_code,
            failed_call: script.failed_call.clone(),
            tools: script_tools(script).into_iter().map(String::from).collect(),
        })
    })
}
