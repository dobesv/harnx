//! `read_exec_log` for a script: the selected lines as plain text, with paging
//! and truncation notes beside it rather than inside it.

use super::log_read::{offset_error, LoadedLog};
use super::script_pending::{read_lossy, structured};
use super::script_result::{cut_stream, StreamText};
use super::{
    format_size, tool_error, BashServer, CallToolResult, ErrorData, ReadExecLogParams, TruncateOpts,
};
use harnx_toolset::ScriptOutput;
use serde_json::json;

/// `read_exec_log` prints the selected lines, like `cat`.
pub(crate) fn read_log_script_output() -> ScriptOutput {
    ScriptOutput {
        stdout: Some("text".to_string()),
        stderr: Some("notes".to_string()),
        summary: Some("log lines, like cat".to_string()),
        ..Default::default()
    }
}

/// One selected log line as text: numbered as `N: line` or bare.
pub(crate) fn log_line(number: usize, line: &str, numbered: bool) -> String {
    if numbered {
        format!("{number}: {line}")
    } else {
        line.to_string()
    }
}

impl BashServer {
    // qual:allow(iosp) — checks the offset against the matching lines, then selects and reports, as the model-style read does.
    pub(super) async fn script_read_exec_log(
        &self,
        params: ReadExecLogParams,
    ) -> Result<CallToolResult, ErrorData> {
        let path = self.open_log(&params).await?;
        let content = read_lossy(&path).await;
        let LoadedLog {
            path,
            lines,
            mut notices,
        } = Self::filter_log_lines(path, &content, &params)?;
        let total_lines = lines.len();
        if total_lines == 0 {
            notices.extend(
                params
                    .grep
                    .iter()
                    .map(|pattern| format!("no lines matched grep pattern '{pattern}'")),
            );
        } else if let Some(message) = offset_error(&path, params.offset, total_lines) {
            return tool_error(message);
        }
        let (selected, selection_notes) = Self::select_checked_log_lines(lines, &params)?;
        notices.extend(selection_notes);
        Ok(log_result(LogReport {
            params: &params,
            selected,
            notes: notices,
            total_lines,
        }))
    }
}

struct LogReport<'a> {
    params: &'a ReadExecLogParams,
    selected: Vec<(usize, String)>,
    notes: Vec<String>,
    total_lines: usize,
}

/// The note that says where to continue a forward read that stopped before
/// the last matching line. `printed` is how many whole lines the result kept.
fn continuation_note(
    params: &ReadExecLogParams,
    printed: usize,
    total_lines: usize,
) -> Option<String> {
    if params.tail.is_some() {
        return None;
    }
    let next = params.offset.unwrap_or(1) + printed;
    (next <= total_lines).then(|| {
        format!(
            "{} more lines. Use --offset {next} to continue",
            total_lines - next + 1
        )
    })
}

fn log_result(report: LogReport<'_>) -> CallToolResult {
    let LogReport {
        params,
        selected,
        mut notes,
        total_lines,
    } = report;
    // A script reads plain lines unless it asks for numbers.
    let numbered = params.line_numbers.unwrap_or(false);
    let text: String = selected
        .iter()
        .map(|(number, line)| format!("{}\n", log_line(*number, line, numbered)))
        .collect();
    let cap = params
        .max_output_bytes
        .unwrap_or_else(|| TruncateOpts::default().max_output_bytes);
    let StreamText { text, total_bytes } = cut_stream(&text, cap);
    let cut_mid_line = !text.is_empty() && !text.ends_with('\n');
    if text.len() < total_bytes {
        notes.push(format!(
            "output cut at {} of {total_bytes} bytes; raise max_output_bytes or page with offset and limit",
            text.len()
        ));
    }
    let kept = text
        .matches('\n')
        .count()
        .max(usize::from(!text.is_empty()));
    let kept_lines = &selected[..kept.min(selected.len())];
    // A line cut in the middle was not printed whole, so no offset continues it.
    if !cut_mid_line {
        notes.extend(continuation_note(params, kept_lines.len(), total_lines));
    }
    let summary = format!(
        "Read {}/{} ({total_lines} lines, {})",
        params.execution_id,
        params.stream,
        format_size(text.len())
    );
    structured(
        json!({
            "execution_id": params.execution_id,
            "stream": params.stream,
            "text": text,
            "notes": notes,
            "start_line": kept_lines.first().map(|(number, _)| number),
            "end_line": kept_lines.last().map(|(number, _)| number),
            "total_lines": total_lines,
        }),
        summary,
    )
}
