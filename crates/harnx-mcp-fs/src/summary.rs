use harnx_mcp::safety::{
    format_size, truncate_output, TruncateOpts, DEFAULT_MAX_BYTES, LS_SCAN_HARD_LIMIT,
};
use rmcp::model::{CallToolResult, Content, ErrorData, Role};
use std::fmt::Write as _;

use crate::server::ReadFileParams;

pub(crate) fn render_read_result(
    params: &ReadFileParams,
    numbered_lines: Vec<(usize, &str)>,
    total_lines: usize,
    shown_start: usize,
    shown_end: usize,
    file_bytes: usize,
    mut notices: Vec<String>,
) -> Result<CallToolResult, ErrorData> {
    let shown_content_bytes: usize = numbered_lines.iter().map(|(_, l)| l.len() + 1).sum();
    let raw_output = numbered_lines
        .into_iter()
        .map(|(n, l)| format!("{n}: {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    let d = TruncateOpts::default();
    let opts = TruncateOpts {
        head_lines: params.head_lines.unwrap_or(d.head_lines),
        tail_lines: params.tail_lines.unwrap_or(d.tail_lines),
        max_output_bytes: params
            .max_output_bytes
            .unwrap_or(d.max_output_bytes.min(DEFAULT_MAX_BYTES)),
        ..d
    };
    let (output, byte_truncated) = truncate_with_notices(
        raw_output,
        &opts,
        "Use head_lines, tail_lines, or max_output_bytes to see more",
        &mut notices,
    );
    let slice = ReadSlice {
        total_lines,
        shown_start,
        shown_end,
        shown_content_bytes,
        file_bytes,
        byte_truncated,
    };
    let summary = read_file_summary(&params.path, &slice);
    Ok(CallToolResult::success(vec![
        Content::text(output).with_audience(vec![Role::Assistant]),
        Content::text(summary).with_audience(vec![Role::User]),
    ]))
}

/// Truncate `raw_output`, append a notice if truncation occurred, then append
/// all accumulated notices to the output.  Returns `(output, byte_truncated)`.
fn truncate_with_notices(
    raw_output: String,
    opts: &TruncateOpts,
    truncation_hint: &str,
    notices: &mut Vec<String>,
) -> (String, bool) {
    let truncated = truncate_output(&raw_output, opts);
    let byte_truncated = truncated != raw_output;
    if byte_truncated {
        notices.push(format!(
            "output truncated from {} to {}. {}",
            format_size(raw_output.len()),
            format_size(truncated.len()),
            truncation_hint,
        ));
    }
    let mut output = truncated;
    if !notices.is_empty() {
        let _ = write!(output, "\n\n[{}]", notices.join(". "));
    }
    (output, byte_truncated)
}

struct ReadSlice {
    total_lines: usize,
    shown_start: usize,
    shown_end: usize,
    shown_content_bytes: usize,
    file_bytes: usize,
    byte_truncated: bool,
}

fn read_file_summary(path: &str, s: &ReadSlice) -> String {
    let all = s.shown_start == 1 && s.shown_end == s.total_lines;
    let lines_part = if all {
        format!("{} lines", s.total_lines)
    } else {
        format!(
            "lines {}\u{2013}{} of {}",
            s.shown_start, s.shown_end, s.total_lines
        )
    };
    let bytes_part = if s.byte_truncated {
        format!(
            "truncated to {} of {}",
            format_size(s.shown_content_bytes),
            format_size(s.file_bytes)
        )
    } else if all {
        format_size(s.file_bytes)
    } else {
        format!(
            "{} of {}",
            format_size(s.shown_content_bytes),
            format_size(s.file_bytes)
        )
    };
    format!("Read {} ({}, {})", path, lines_part, bytes_part)
}

pub(crate) fn ls_summary(
    path: &str,
    entry_count: usize,
    scan_count: usize,
    limit_reached: bool,
) -> String {
    if scan_count >= LS_SCAN_HARD_LIMIT {
        format!(
            "Listed {} of {}+ entries in {}",
            entry_count, LS_SCAN_HARD_LIMIT, path
        )
    } else if limit_reached {
        format!(
            "Listed {} of {} entries in {}",
            entry_count, scan_count, path
        )
    } else {
        format!("Listed {} entries in {}", entry_count, path)
    }
}

pub(crate) fn apply_search_notices(
    raw_output: String,
    limit_reached: bool,
    max_results: usize,
) -> (String, bool) {
    let mut notices = Vec::new();
    if limit_reached {
        notices.push(format!(
            "results limited to {} matches. Refine the pattern for more specific results",
            max_results
        ));
    }
    truncate_with_notices(
        raw_output,
        &TruncateOpts::default(),
        "Refine the pattern or narrow the search path",
        &mut notices,
    )
}

pub(crate) struct SearchTruncation {
    pub match_count: usize,
    pub max_results: usize,
    pub limit_reached: bool,
    pub output_bytes: usize,
    pub raw_bytes: usize,
    pub byte_truncated: bool,
}

pub(crate) fn search_summary(location: &str, t: &SearchTruncation) -> String {
    match (t.limit_reached, t.byte_truncated) {
        (false, false) => format!("Found {} matches in {}", t.match_count, location),
        (true, false) => format!(
            "Found {}+ matches in {} (showing {})",
            t.max_results, location, t.match_count
        ),
        (false, true) => format!(
            "Found {} matches in {} ({} of {} shown)",
            t.match_count,
            location,
            format_size(t.output_bytes),
            format_size(t.raw_bytes)
        ),
        (true, true) => format!(
            "Found {}+ matches in {} (showing {}, truncated to {} of {})",
            t.max_results,
            location,
            t.match_count,
            format_size(t.output_bytes),
            format_size(t.raw_bytes)
        ),
    }
}

pub(crate) fn find_summary(path_count: usize, max_results: usize, limit_reached: bool) -> String {
    if limit_reached {
        format!("Found {}+ matches (showing {})", max_results, path_count)
    } else {
        format!("Found {} matches", path_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_file_summary_complete() {
        let s = read_file_summary(
            "foo.txt",
            &ReadSlice {
                total_lines: 10,
                shown_start: 1,
                shown_end: 10,
                shown_content_bytes: 100,
                file_bytes: 100,
                byte_truncated: false,
            },
        );
        assert!(s.starts_with("Read foo.txt ("), "got: {s:?}");
        assert!(
            !s.contains("lines 1"),
            "complete read should not show range, got: {s:?}"
        );
    }

    #[test]
    fn read_file_summary_paginated() {
        let s = read_file_summary(
            "bar.txt",
            &ReadSlice {
                total_lines: 100,
                shown_start: 1,
                shown_end: 20,
                shown_content_bytes: 200,
                file_bytes: 1000,
                byte_truncated: false,
            },
        );
        assert!(s.contains("lines 1") && s.contains("of 100"), "got: {s:?}");
        assert!(s.contains("of 1"), "expected byte ratio, got: {s:?}");
    }
}
