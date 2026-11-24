//! Results a script reads for a command that has not finished, for a spawned
//! command's final state, and for a log read. They sit beside
//! `script_result.rs`, which holds the finished-command report they build on.

use super::script_result::{
    command_result, ended_with, read_log_command, status_of, CommandReport,
};
use super::supervisor::ProcessExit;
use super::{BashServer, ExitKind, Path, Regex, SpawnedProcess, TruncateOpts};
use harnx_toolset_server::content::WithAudience;
use rmcp::model::{CallToolResult, ContentBlock, Role};
use serde_json::{json, Value};

/// A command that has not finished. The result has no `stdout` field, so the
/// mapping prints these fields as one JSON line.
pub(crate) struct PendingReport<'a> {
    pub execution_id: &'a str,
    /// `spawned` or `running`. A spawned command past its own deadline has
    /// finished and reports through `command_result`.
    pub status: &'a str,
    pub running_for_secs: Option<u64>,
    /// `new_message` or `caller_deadline`, when that ended the wait.
    pub stopped_by: Option<&'a str>,
    pub stdout_log_path: &'a str,
    pub stderr_log_path: &'a str,
    /// Notes beyond the one a running command always carries.
    pub extra_notes: Vec<String>,
}

/// The status reported for a process that left neither a code nor a signal.
const UNKNOWN_STATUS: i32 = 255;
const RUNNING_NOTE: &str = "still running; stdout is its status as JSON";

pub(crate) fn pending_result(report: PendingReport<'_>, summary: String) -> CallToolResult {
    let mut notes: Vec<&str> = if report.status == "running" {
        vec![RUNNING_NOTE]
    } else {
        vec![]
    };
    notes.extend(report.extra_notes.iter().map(String::as_str));
    let mut fields = json!({
        "execution_id": report.execution_id,
        "status": report.status,
        "notes": notes,
        "stdout_log_path": report.stdout_log_path,
        "stderr_log_path": report.stderr_log_path,
    });
    if let Some(secs) = report.running_for_secs {
        fields["running_for_secs"] = json!(secs);
    }
    if let Some(stopped_by) = report.stopped_by {
        fields["stopped_by"] = json!(stopped_by);
    }
    structured(fields, summary)
}

pub(super) fn structured(fields: Value, summary: String) -> CallToolResult {
    let mut result = CallToolResult::success(vec![
        ContentBlock::text(summary).with_audience(vec![Role::User])
    ]);
    result.structured_content = Some(fields);
    result
}

/// What a script-style `wait` or `status` reads and reports.
pub(super) struct ScriptStateCtx<'a> {
    pub execution_id: &'a str,
    pub entry: &'a SpawnedProcess,
    pub max_output_bytes: Option<usize>,
    pub grep: Option<&'a Regex>,
    /// Notes for the line caps the call passed and a script cannot use.
    pub line_cap_notes: Vec<String>,
}

/// The log's lines that match `grep`, each keeping its newline.
fn matching_lines(text: String, grep: Option<&Regex>) -> String {
    let Some(regex) = grep else { return text };
    text.split_inclusive('\n')
        .filter(|line| regex.is_match(line.trim_end_matches('\n')).unwrap_or(false))
        .collect()
}

/// A log as text. Bytes that are not UTF-8 become U+FFFD rather than an
/// empty log.
pub(super) async fn read_lossy(path: &Path) -> String {
    let bytes = tokio::fs::read(path).await.unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

impl BashServer {
    /// A spawned command's final state, reported like a finished `exec`.
    pub(super) async fn script_finished(
        &self,
        ctx: ScriptStateCtx<'_>,
        exit: &ProcessExit,
        summary: impl FnOnce(ExitKind, i32) -> String,
    ) -> CallToolResult {
        let entry = ctx.entry;
        let kind = entry.exit_kind(exit);
        let code = status_of(exit.code, exit.signal).unwrap_or(UNKNOWN_STATUS);
        let mut ended = ended_with(kind, code, exit.signal);
        ended.notes.extend(ctx.line_cap_notes.iter().cloned());
        let stdout = matching_lines(read_lossy(&entry.stdout_log_path).await, ctx.grep);
        let stderr = matching_lines(read_lossy(&entry.stderr_log_path).await, ctx.grep);
        let diffs = entry
            .exit_diff
            .get_or_init(|| {
                self.diff_snapshots(
                    &entry.before_snap_ids,
                    &entry.snapshot_decision,
                    &entry.working_dir,
                    "after wait",
                )
            })
            .await
            .clone();
        let summary = summary(kind, ended.exit_code);
        command_result(
            CommandReport {
                execution_id: ctx.execution_id,
                status: ended.status,
                exit_code: ended.exit_code,
                stdout: &stdout,
                stderr: &stderr,
                stdout_log_path: &entry.stdout_log_path.to_string_lossy(),
                stderr_log_path: &entry.stderr_log_path.to_string_lossy(),
                max_output_bytes: ctx
                    .max_output_bytes
                    .unwrap_or_else(|| TruncateOpts::default().max_output_bytes),
                log_command: Some(read_log_command()),
                log_unfiltered: ctx.grep.is_none(),
                notes: ended.notes,
                diffs,
            },
            summary,
        )
    }
}
