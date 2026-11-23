//! Results shaped for a bash script: plain text in structured fields, with a
//! one-line summary for the person watching. The model-facing results stay in
//! `summary.rs`.

use harnx_core::safety::{format_size, TruncateOpts};
use harnx_toolset_server::content::WithAudience;
use rmcp::model::{CallToolResult, ContentBlock, Role};
use serde_json::{json, Value};

use crate::server::ReadFileParams;
use crate::summary::{read_file_summary, ReadSlice};

/// Who a result is rendered for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResultStyle {
    /// Text and notices written for a model to read.
    #[default]
    Model,
    /// Plain data in `structuredContent`, for a script to pipe.
    Script,
}

/// One user-audience summary block plus the fields in structuredContent.
pub(crate) fn script_result(fields: Value, summary: String) -> CallToolResult {
    let mut result = CallToolResult::success(vec![
        ContentBlock::text(summary).with_audience(vec![Role::User])
    ]);
    result.structured_content = Some(fields);
    result
}

/// The lines `read` selected, before any size limit is applied.
pub(crate) struct ReadSelection<'a> {
    pub lines: Vec<(usize, &'a str)>,
    pub total_lines: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub file_bytes: usize,
    pub notes: Vec<String>,
    /// The file's last line, when the file does not end with a newline.
    pub unterminated_line: Option<usize>,
}

/// A read that matched nothing: empty text and no notes.
pub(crate) fn empty_read_result(path: &str, summary: String) -> CallToolResult {
    let place = Place {
        path,
        total_lines: 0,
        start_line: 0,
        end_line: 0,
    };
    script_result(read_fields("", &[], &place), summary)
}

/// Which file and which lines of it a result covers.
struct Place<'a> {
    path: &'a str,
    total_lines: usize,
    start_line: usize,
    end_line: usize,
}

pub(crate) fn script_read_result(
    params: &ReadFileParams,
    selection: ReadSelection<'_>,
) -> CallToolResult {
    let ReadSelection {
        mut lines,
        total_lines,
        start_line,
        mut end_line,
        file_bytes,
        mut notes,
        unterminated_line,
    } = selection;
    let omitted = apply_head_tail(&mut lines, params);
    if omitted > 0 {
        notes.push(format!("{omitted} lines left out"));
    }
    let mut rows = lines
        .iter()
        .map(|(n, l)| line_text(params, *n, l, unterminated_line))
        .collect::<Vec<_>>();
    let cap = params
        .max_output_bytes
        .unwrap_or(TruncateOpts::default().max_output_bytes);
    let bound = Bound {
        cap,
        start_line,
        total_lines,
        contiguous: omitted == 0,
    };
    let (byte_truncated, new_end) = bound_rows(&mut rows, &bound, &mut notes);
    end_line = new_end.unwrap_or(end_line);
    let text = rows.concat();
    let slice = ReadSlice {
        total_lines,
        shown_start: start_line,
        shown_end: end_line,
        shown_content_bytes: text.len(),
        file_bytes,
        byte_truncated,
    };
    let summary = read_file_summary(&params.path, &slice);
    let place = Place {
        path: &params.path,
        total_lines,
        start_line,
        end_line,
    };
    let fields = read_fields(&text, &notes, &place);
    script_result(fields, summary)
}

/// One selected line as the script sees it: newline-terminated unless it is
/// the file's unterminated last line.
fn line_text(
    params: &ReadFileParams,
    number: usize,
    line: &str,
    unterminated_line: Option<usize>,
) -> String {
    let mut row = if params.line_numbers == Some(true) {
        format!("{number}: {line}")
    } else {
        line.to_string()
    };
    if unterminated_line != Some(number) {
        row.push('\n');
    }
    row
}

fn read_fields(text: &str, notes: &[String], place: &Place<'_>) -> Value {
    json!({
        "text": text,
        "notes": notes,
        "path": place.path,
        "total_lines": place.total_lines,
        "start_line": place.start_line,
        "end_line": place.end_line,
    })
}

/// Keep the first `head_lines` and last `tail_lines` lines when the caller set
/// either, with nothing between. Returns how many lines were dropped.
fn apply_head_tail(lines: &mut Vec<(usize, &str)>, params: &ReadFileParams) -> usize {
    if params.head_lines.is_none() && params.tail_lines.is_none() {
        return 0;
    }
    let head = params.head_lines.unwrap_or(0);
    let tail = params.tail_lines.unwrap_or(0);
    if head + tail >= lines.len() {
        return 0;
    }
    let omitted = lines.len() - head - tail;
    lines.drain(head..lines.len() - tail);
    omitted
}

struct Bound {
    cap: usize,
    start_line: usize,
    total_lines: usize,
    /// The rows are consecutive lines, so `offset` can continue after them.
    contiguous: bool,
}

/// Keep whole rows from the start that fit in the cap. A first row that does
/// not fit alone is cut at a character boundary, so the text never exceeds the
/// cap. Returns whether anything was dropped and the new last line, when the
/// rows are consecutive.
fn bound_rows(
    rows: &mut Vec<String>,
    bound: &Bound,
    notes: &mut Vec<String>,
) -> (bool, Option<usize>) {
    let mut size = 0;
    let kept = rows
        .iter()
        .take_while(|row| {
            size += row.len();
            size <= bound.cap
        })
        .count();
    if kept == rows.len() {
        return (false, None);
    }
    let limit = format_size(bound.cap);
    let end_line = (bound.start_line + kept.max(1) - 1).min(bound.total_lines);
    if kept == 0 {
        let cut = (0..=bound.cap.min(rows[0].len()))
            .rev()
            .find(|&at| rows[0].is_char_boundary(at))
            .unwrap_or(0);
        rows.truncate(1);
        rows[0].truncate(cut);
        let skip = if bound.contiguous {
            format!(". Use offset={} to skip the rest of it", end_line + 1)
        } else {
            String::new()
        };
        notes.push(format!(
            "line {} is longer than {limit}; showing its first {limit}{skip}",
            end_line
        ));
    } else {
        rows.truncate(kept);
        notes.push(if bound.contiguous {
            format!(
                "output stopped at line {end_line} of {} to stay within {limit}. Use offset={} to continue",
                bound.total_lines,
                end_line + 1
            )
        } else {
            format!("output stopped after {kept} lines to stay within {limit}")
        });
    }
    (true, bound.contiguous.then_some(end_line))
}
