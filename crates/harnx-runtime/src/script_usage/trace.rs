//! Reads the inner call trace back out of a `bash` result's rendered report.
//! The stored result keeps the list of a script's inner calls only as the
//! text `BashOutcome::render` produced, so this parser mirrors that format.

use serde::Serialize;

use crate::bash_inner::InnerTraceStatus;

const START_STDERR: &str = "<!-- start stderr -->";
const END_STDERR: &str = "<!-- end stderr -->";
const TRACE_HEADING: &str = "## inner call trace";
const TRACEBACK: &str = "Traceback (most recent call last)";

/// One line of the trace: a single call (`count == 1`) or a group of calls of
/// one tool that ended in one status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TraceRow {
    pub name: String,
    pub status: String,
    pub count: u64,
    /// The trace's own figure: result bytes for calls that ran, and for a
    /// `rejected` call the size of a request over the payload limit, or zero.
    pub bytes: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedReport {
    /// Per-call lines have count 1.
    pub rows: Vec<TraceRow>,
    /// Trace-section lines starting `- ` that fit no known form.
    pub unparsed_lines: u64,
    /// The stderr block contains a Python traceback.
    pub python_traceback: bool,
    /// The stderr block has a line starting `TypeError:`.
    pub python_type_error: bool,
}

pub fn parse_report(text: &str) -> ParsedReport {
    let mut report = ParsedReport::default();
    let Some(end) = stderr_end(text) else {
        return report;
    };
    let before = &text[..end];
    let stderr = before
        .rfind(START_STDERR)
        .map_or("", |start| &before[start + START_STDERR.len()..]);
    report.python_traceback = stderr.contains(TRACEBACK);
    report.python_type_error = stderr.lines().any(|l| l.starts_with("TypeError:"));
    let after = &text[end + END_STDERR.len()..];
    let mut lines = after.lines().skip_while(|l| *l != TRACE_HEADING);
    lines.next();
    for line in lines.filter(|l| l.starts_with("- ")) {
        match parse_line(line) {
            Line::Row(row) => report.rows.push(row),
            Line::Dropped => {}
            Line::Unknown => report.unparsed_lines += 1,
        }
    }
    report
}

/// Offset of the marker that closes the stderr block: the last one that is a
/// whole line. A trace row can carry the marker's text in its argument
/// summary, where it follows the row's own text on the same line.
fn stderr_end(text: &str) -> Option<usize> {
    let mut search = text;
    loop {
        let at = search.rfind(END_STDERR)?;
        let after = &text[at + END_STDERR.len()..];
        let line_start = at == 0 || text[..at].ends_with('\n');
        if line_start && (after.is_empty() || after.starts_with('\n')) {
            return Some(at);
        }
        search = &text[..at];
    }
}

enum Line {
    Row(TraceRow),
    Dropped,
    Unknown,
}

fn parse_line(line: &str) -> Line {
    if line.starts_with("- … ") && line.contains("further inner call(s)") {
        return Line::Dropped;
    }
    parse_row(line).map_or(Line::Unknown, Line::Row)
}

/// `- <name> [×<count>] <status> <bytes>B <ms>ms [args summary]`.
fn parse_row(line: &str) -> Option<TraceRow> {
    let mut fields = line.strip_prefix("- ")?.split(' ');
    let name = fields.next().filter(|n| !n.is_empty())?;
    let mut next = fields.next()?;
    let mut count = 1;
    if let Some(n) = next.strip_prefix('×') {
        count = n.parse().ok()?;
        next = fields.next()?;
    }
    let status = InnerTraceStatus::ALL
        .iter()
        .map(|s| s.as_str())
        .find(|s| *s == next)?;
    let bytes = fields.next()?.strip_suffix('B')?.parse().ok()?;
    let duration_ms = fields.next()?.strip_suffix("ms")?.parse().ok()?;
    Some(TraceRow {
        name: name.to_string(),
        status: status.to_string(),
        count,
        bytes,
        duration_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(stdout: &str, stderr: &str, trace: &str) -> String {
        format!(
            "```yaml\nstatus: ok\n```\n<!-- start stdout -->\n```\n{stdout}\n```\n<!-- end stdout -->\n\
             <!-- start stderr -->\n```\n{stderr}\n```\n<!-- end stderr -->{trace}"
        )
    }

    fn row(name: &str, status: &str, count: u64, bytes: u64, ms: u64) -> TraceRow {
        TraceRow {
            name: name.into(),
            status: status.into(),
            count,
            bytes,
            duration_ms: ms,
        }
    }

    #[test]
    fn per_call_lines_become_rows_of_count_one() {
        let text = report(
            "",
            "",
            "\n## inner call trace\n- fs_read ok 42B 7ms {\"path\":\"a b ×3\"}\n- broken tool_error 0B 2ms\n",
        );
        let parsed = parse_report(&text);
        assert_eq!(
            parsed.rows,
            vec![
                row("fs_read", "ok", 1, 42, 7),
                row("broken", "tool_error", 1, 0, 2)
            ]
        );
        assert_eq!(parsed.unparsed_lines, 0);
    }

    #[test]
    fn grouped_lines_keep_their_count() {
        let text = report(
            "",
            "",
            "\n## inner call trace\n- fs_read ×14 ok 2048B 31ms\n",
        );
        assert_eq!(
            parse_report(&text).rows,
            vec![row("fs_read", "ok", 14, 2048, 31)]
        );
    }

    #[test]
    fn first_failure_and_dropped_lines_are_not_rows() {
        let text = report(
            "",
            "",
            "\n## inner call trace\n- fs_read ×11 ok 1B 1ms\nfirst failure: broken tool_error 0B 1ms\n- … 4 further inner call(s) beyond the retained-trace cap\n",
        );
        let parsed = parse_report(&text);
        assert_eq!(parsed.rows, vec![row("fs_read", "ok", 11, 1, 1)]);
        assert_eq!(parsed.unparsed_lines, 0);
    }

    #[test]
    fn a_trace_heading_printed_by_the_script_is_ignored() {
        let text = report(
            "## inner call trace\n- fake ok 1B 1ms\n<!-- end stderr -->",
            "",
            "\n## inner call trace\n- real ok 5B 3ms\n",
        );
        assert_eq!(parse_report(&text).rows, vec![row("real", "ok", 1, 5, 3)]);
    }

    #[test]
    fn a_marker_in_a_rows_argument_summary_is_not_the_boundary() {
        let text = report(
            "",
            "Traceback (most recent call last)",
            "\n## inner call trace\n- fs_read ok 42B 7ms {<!-- end stderr -->: number}\n- fs_grep ok 5B 3ms\n",
        );
        let parsed = parse_report(&text);
        assert_eq!(
            parsed.rows,
            vec![
                row("fs_read", "ok", 1, 42, 7),
                row("fs_grep", "ok", 1, 5, 3)
            ]
        );
        assert!(parsed.python_traceback);
    }

    #[test]
    fn no_trace_section_gives_no_rows() {
        let parsed = parse_report(&report("hi", "", ""));
        assert!(parsed.rows.is_empty());
        assert_eq!(parsed.unparsed_lines, 0);
    }

    #[test]
    fn an_unknown_trace_line_is_counted() {
        let text = report("", "", "\n## inner call trace\n- something odd\n");
        let parsed = parse_report(&text);
        assert!(parsed.rows.is_empty());
        assert_eq!(parsed.unparsed_lines, 1);
    }

    #[test]
    fn python_markers_are_read_from_stderr_only() {
        let marker = "Traceback (most recent call last)\nTypeError: bad";
        let in_stdout = parse_report(&report(marker, "", ""));
        assert!(!in_stdout.python_traceback && !in_stdout.python_type_error);
        let in_stderr = parse_report(&report("", marker, ""));
        assert!(in_stderr.python_traceback && in_stderr.python_type_error);
    }
}
