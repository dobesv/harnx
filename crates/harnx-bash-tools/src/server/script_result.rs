//! Results shaped for a bash script: the command's stdout, stderr and exit
//! status as separate fields, the way `bash -c` reports them. Model-facing
//! results stay in `render.rs`.

use harnx_toolset::ScriptOutput;
use harnx_toolset_server::content::WithAudience;
use rmcp::model::{CallToolResult, ContentBlock, Role};
use serde_json::json;
use std::process::ExitStatus;

use super::ExitKind;

/// A stream cut to the whole lines that fit the output cap.
pub(crate) struct StreamText {
    pub text: String,
    pub total_bytes: usize,
}

/// What a finished or stopped command reports to a script.
pub(crate) struct CommandReport<'a> {
    pub execution_id: &'a str,
    /// `exited`, `timeout` or `interrupted`.
    pub status: &'a str,
    pub exit_code: i32,
    pub stdout: &'a str,
    pub stderr: &'a str,
    pub stdout_log_path: &'a str,
    pub stderr_log_path: &'a str,
    pub max_output_bytes: usize,
    /// The command that reads a full log, named in a cut stream's note.
    /// `None` for results that have no such tool, such as templates.
    pub log_command: Option<String>,
    /// Whether the streams are the log's own lines, so a line count into the
    /// log is also a line offset for `log_command`. False when a filter chose
    /// the lines.
    pub log_unfiltered: bool,
    pub notes: Vec<String>,
    /// History diffs the model-facing result appends as extra text blocks.
    pub diffs: Vec<String>,
}

/// Exit statuses a shell uses for a command a signal or a deadline ended.
const SIGNAL_STATUS_BASE: i32 = 128;
const TIMEOUT_STATUS: i32 = 124;
const INTERRUPTED_STATUS: i32 = 130;

/// How a command ended, in the terms a script reads.
pub(crate) struct Ended {
    pub status: &'static str,
    pub exit_code: i32,
    pub notes: Vec<String>,
}

/// The command's status as a shell reports it: its exit code, or 128 plus the
/// signal number when a signal ended it.
pub(crate) fn shell_status(status: ExitStatus) -> i32 {
    status_of(status.code(), signal_of(status)).unwrap_or(-1)
}

/// The shell status for an exit code and the signal that ended the process;
/// `None` when neither is known.
pub(crate) fn status_of(code: Option<i32>, signal: Option<i32>) -> Option<i32> {
    code.or(signal.map(|signal| SIGNAL_STATUS_BASE + signal))
}

#[cfg(unix)]
pub(crate) fn signal_of(status: ExitStatus) -> Option<i32> {
    std::os::unix::process::ExitStatusExt::signal(&status)
}

#[cfg(not(unix))]
pub(crate) fn signal_of(_status: ExitStatus) -> Option<i32> {
    None
}

impl Ended {
    fn new(status: &'static str, exit_code: i32, notes: Vec<String>) -> Self {
        Self {
            status,
            exit_code,
            notes,
        }
    }
}

pub(crate) fn ended(kind: ExitKind, status: ExitStatus) -> Ended {
    ended_with(kind, shell_status(status), signal_of(status))
}

/// `ended` for a process whose exit status was recorded as a code and a signal.
pub(crate) fn ended_with(kind: ExitKind, exit_code: i32, signal: Option<i32>) -> Ended {
    match kind {
        ExitKind::TimedOut { deadline_secs } => Ended::new(
            "timeout",
            TIMEOUT_STATUS,
            vec![format!(
                "timed out after {deadline_secs}s and was terminated"
            )],
        ),
        ExitKind::Interrupted => Ended::new(
            "interrupted",
            INTERRUPTED_STATUS,
            vec!["stopped by a new message".to_string()],
        ),
        ExitKind::Exited => Ended::new(
            "exited",
            exit_code,
            signal
                .map(|signal| format!("ended by signal {signal}"))
                .into_iter()
                .collect(),
        ),
    }
}

/// Whole lines from the start of `text` up to `cap` bytes. A first line longer
/// than the cap is cut at the last character boundary that fits.
pub(crate) fn cut_stream(text: &str, cap: usize) -> StreamText {
    let total_bytes = text.len();
    if total_bytes <= cap {
        return StreamText {
            text: text.to_string(),
            total_bytes,
        };
    }
    let mut end = 0;
    for line in text.split_inclusive('\n') {
        if end + line.len() > cap {
            break;
        }
        end += line.len();
    }
    if end == 0 {
        end = (0..=cap)
            .rev()
            .find(|&index| text.is_char_boundary(index))
            .unwrap_or(0);
    }
    StreamText {
        text: text[..end].to_string(),
        total_bytes,
    }
}

fn cut_note(name: &str, cut: &StreamText, report: &CommandReport<'_>) -> Option<String> {
    if cut.text.len() >= cut.total_bytes {
        return None;
    }
    let mut note = format!(
        "{name} cut at {} of {} bytes",
        cut.text.len(),
        cut.total_bytes
    );
    if let Some(command) = &report.log_command {
        note.push_str(&format!(
            "; full log: {command} --execution-id {} --stream {name}",
            report.execution_id
        ));
        // A line cut in the middle has no offset that continues it.
        if report.log_unfiltered && cut.text.ends_with('\n') {
            note.push_str(&format!(" --offset {}", cut.text.matches('\n').count() + 1));
        }
    }
    Some(note)
}

/// The command that reads a full log back, named as the host registers it.
pub(crate) fn read_log_command() -> String {
    format!("{}_read_exec_log", crate::toolset::TOOLSET_NAME)
}

pub(crate) fn command_result(report: CommandReport<'_>, summary: String) -> CallToolResult {
    let stdout = cut_stream(report.stdout, report.max_output_bytes);
    let stderr = cut_stream(report.stderr, report.max_output_bytes);
    let mut notes = report.notes.clone();
    notes.extend(cut_note("stdout", &stdout, &report));
    notes.extend(cut_note("stderr", &stderr, &report));
    let mut fields = json!({
        "execution_id": report.execution_id,
        "status": report.status,
        "exit_code": report.exit_code,
        "stdout": stdout.text,
        "stderr": stderr.text,
        "notes": notes,
        "stdout_log_path": report.stdout_log_path,
        "stderr_log_path": report.stderr_log_path,
    });
    if !report.diffs.is_empty() {
        fields["diffs"] = json!(report.diffs);
    }
    let mut result = CallToolResult::success(vec![
        ContentBlock::text(summary).with_audience(vec![Role::User])
    ]);
    result.structured_content = Some(fields);
    result
}

/// `exec` and plain command templates read like `bash -c`.
pub(crate) fn exec_script_output() -> ScriptOutput {
    ScriptOutput {
        summary: Some("the command's stdout, stderr and exit status, like bash -c".to_string()),
        ..command_mapping()
    }
}

/// `spawn`, `wait` and `status` print a finished command like `bash -c` and an
/// unfinished one as status JSON.
pub(crate) fn pending_script_output() -> ScriptOutput {
    ScriptOutput {
        summary: Some(
            "a finished command's stdout, stderr and exit status; otherwise one line of status JSON"
                .to_string(),
        ),
        ..command_mapping()
    }
}

fn command_mapping() -> ScriptOutput {
    ScriptOutput {
        stdout: Some("stdout".to_string()),
        stderr_text: Some("stderr".to_string()),
        stderr: Some("notes".to_string()),
        exit_code: Some("exit_code".to_string()),
        error_exit_code: Some(125),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests;
