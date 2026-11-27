//! Results shaped for a bash script: the text a script can pipe in
//! `structuredContent`, and paging or cut-off notices apart from it. The
//! model-facing results stay in `handlers.rs`.

use harnx_core::safety::{format_size, TruncateOpts};
use harnx_toolset_server::content::WithAudience;
use rmcp::model::{CallToolResult, ContentBlock, Role};
use serde_json::{json, Value};

use crate::handlers::{AttachmentReadParams, BytePage};

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
fn script_result(fields: Value, summary: String) -> CallToolResult {
    let mut result = CallToolResult::success(vec![
        ContentBlock::text(summary).with_audience(vec![Role::User])
    ]);
    result.structured_content = Some(fields);
    result
}

/// A byte page for a script: the raw UTF-8 slice as `text`, continuation
/// metadata as fields, and a readable note on stderr while content remains.
pub(crate) fn script_byte_page(
    page: &BytePage,
    text: &str,
    url: &str,
    mime_type: &str,
) -> CallToolResult {
    let end_of_content = page.end >= page.total;
    let notes = (!end_of_content)
        .then(|| {
            format!(
                "{} bytes left. Use byte_offset={} to continue",
                page.total - page.end,
                page.end
            )
        })
        .into_iter()
        .collect::<Vec<_>>();
    let fields = json!({
        "text": text,
        "notes": notes,
        "url": url,
        "content_type": mime_type,
        "byte_offset": page.start,
        "next_byte_offset": (!end_of_content).then_some(page.end),
        "end_of_content": end_of_content,
        "total_bytes": page.total,
    });
    script_result(fields, page.summary(url))
}

/// A created attachment for a script: the confirmation line on stdout and
/// the URL as a field, so scripts never parse text for the `cid:` URL.
pub(crate) fn script_create_result(url: &str, mime_type: &str) -> CallToolResult {
    // The shell prints `message` verbatim, so it carries its own newline: a
    // `while read` loop sees a complete record and a following command starts
    // on a fresh line. The URL field and the summary stay clean.
    let fields = json!({
        "message": format!("Created attachment: {url}\n"),
        "url": url,
        "mime_type": mime_type,
    });
    let summary = format!("Created attachment: {url}");
    script_result(fields, summary)
}

/// Line-mode read for a script. Unlike the model result there are no in-band
/// truncation markers and no silent head/tail window defaults: the text is
/// clean, what was left out is a note, and every cut names the selector that
/// continues from where the text stopped. `offset` continues in the same
/// line space the caller selected (after `grep` filtering), so a script that
/// repeats its own arguments and follows the note reassembles the content.
/// A cap-cut note without `Use offset=` means the byte cap stopped inside
/// a two-sided head/tail window whose kept blocks are disjoint, so no
/// single continuation line exists — not that the content ended. Notes
/// about grep or window omissions never carry a cursor.
pub(crate) fn script_line_page(
    text: &str,
    params: &AttachmentReadParams,
    mime_type: &str,
    summary: String,
) -> CallToolResult {
    let selection = LineSelection::new(text, params);
    let fields = json!({
        "text": selection.text,
        "notes": selection.notes,
        "url": params.url,
        "content_type": mime_type,
    });
    script_result(fields, summary)
}

/// The lines a script sees, after grep, offset/limit, head/tail and the byte
/// cap, with the notes explaining each cut.
struct LineSelection {
    text: String,
    notes: Vec<String>,
}

/// Where the byte cap stopped, when it stopped before the selection's end.
enum ByteCut {
    /// Whole lines were kept; `kept` is how many.
    Lines { kept: usize },
    /// The first line alone exceeded the cap and shows its first `bytes`.
    FirstLine { bytes: usize },
}

impl LineSelection {
    fn new(text: &str, params: &AttachmentReadParams) -> Self {
        let mut notes = Vec::new();
        // Lines keep their newline, like the model path, so a full read
        // copies the attachment back byte for byte.
        let lines: Vec<&str> = if text.is_empty() {
            Vec::new()
        } else {
            text.split_inclusive('\n').collect()
        };
        let lines = grep_lines(lines, params, &mut notes);
        // offset is 1-indexed in the same line space grep filtered.
        let selection_offset = params.offset.unwrap_or(1).max(1);
        let mut lines = lines
            .into_iter()
            .skip(selection_offset - 1)
            .collect::<Vec<_>>();
        if let Some(limit) = params.limit {
            lines.truncate(limit);
        }
        // Explicit head/tail caps drop the middle of the selection. Without
        // them a script gets whole lines from the start of the selection and
        // the byte cap below decides where the page ends, so no selector
        // silently hides the middle of the output from a script.
        let selection_len = lines.len();
        let window = apply_head_tail_window(&mut lines, params, &mut notes);
        let (text, cut) = bound_lines(&lines, byte_cap(params));
        if let Some(cut) = cut {
            notes.push(byte_cut_note(
                cut,
                byte_cap(params),
                &LineWindow {
                    selection_len,
                    selection_offset,
                    window,
                },
            ));
        }
        Self { text, notes }
    }
}

/// What a head/tail window kept: where the retained text starts within the
/// selection, how many lines it holds, and whether a follow-up `offset`
/// continues from where it stopped. A two-sided window kept two disjoint
/// blocks, so no single offset can name what comes next.
struct WindowInfo {
    first_idx: usize,
    retained_len: usize,
    contiguous: bool,
}

/// Keep the head block, the tail block, or both, and note what was left out.
/// Nothing happens when the caps cover the whole selection.
fn apply_head_tail_window(
    lines: &mut Vec<&str>,
    params: &AttachmentReadParams,
    notes: &mut Vec<String>,
) -> WindowInfo {
    let mut window = WindowInfo {
        first_idx: 0,
        retained_len: lines.len(),
        contiguous: true,
    };
    if params.head_lines.is_none() && params.tail_lines.is_none() {
        return window;
    }
    let head = params.head_lines.unwrap_or(0);
    let tail = params.tail_lines.unwrap_or(0);
    if head.saturating_add(tail) >= lines.len() {
        return window;
    }
    notes.push(format!("{} lines left out", lines.len() - head - tail));
    window.retained_len = head + tail;
    // The retained text starts at the head block when there is one, and at
    // the tail block otherwise.
    if head == 0 {
        window.first_idx = lines.len() - tail;
    }
    if head > 0 && tail > 0 {
        window.contiguous = false;
    }
    let tail_block = lines.split_off(lines.len() - tail);
    lines.truncate(head);
    lines.extend(tail_block);
    window
}

/// Where the byte cap stopped, in the caller's line space.
struct LineWindow {
    /// Lines the caller selected (after grep and offset/limit) before the
    /// head/tail window.
    selection_len: usize,
    /// The 1-indexed `offset` the caller passed; line numbers are named in
    /// the caller's space so repeating the arguments continues correctly.
    selection_offset: usize,
    window: WindowInfo,
}

/// The note for a byte cut. Line numbers are absolute positions in the
/// caller's line space ("at line N"), counts say how many lines the page
/// emitted ("after N of M") — the two never share a number.
fn byte_cut_note(cut: ByteCut, cap: usize, window: &LineWindow) -> String {
    let cap = format_size(cap);
    let WindowInfo {
        first_idx,
        retained_len,
        contiguous,
    } = window.window;
    match cut {
        ByteCut::FirstLine { bytes } => {
            let line = window.selection_offset + first_idx;
            let skip = if contiguous {
                format!(". Use offset={} to skip the rest of it", line + 1)
            } else {
                String::new()
            };
            format!("line {line} is longer than {cap}; showing its first {bytes} bytes{skip}")
        }
        ByteCut::Lines { kept } => {
            if contiguous {
                // Both numbers are absolute lines in the caller's space:
                // `stopped_at` is the next unprinted line, and the
                // selection ends at `selection_offset + selection_len - 1`.
                let stopped_at = window.selection_offset + first_idx + kept;
                let selection_end = window.selection_offset + window.selection_len;
                format!(
                    "output stopped at line {stopped_at} of {} to stay within {cap}. Use offset={stopped_at} to continue",
                    selection_end - 1
                )
            } else {
                format!(
                    "output stopped after {kept} of {retained_len} retained lines to stay within {cap}"
                )
            }
        }
    }
}

fn byte_cap(params: &AttachmentReadParams) -> usize {
    params
        .max_output_bytes
        .unwrap_or(TruncateOpts::default().max_output_bytes)
        .max(1)
}

/// Keep whole lines from the start that fit in `cap` bytes. A first line that
/// does not fit alone is cut at a character boundary, so the text never
/// exceeds the cap. Returns the text and the cut, when anything was dropped.
fn bound_lines(lines: &[&str], cap: usize) -> (String, Option<ByteCut>) {
    let total: usize = lines.iter().map(|line| line.len()).sum();
    if total <= cap {
        return (lines.concat(), None);
    }
    let mut size = 0;
    let kept = lines
        .iter()
        .take_while(|line| {
            size += line.len();
            size <= cap
        })
        .count();
    if kept == 0 {
        let first = lines[0];
        let cut = (0..=cap.min(first.len()))
            .rev()
            .find(|&at| first.is_char_boundary(at))
            .unwrap_or(0);
        (
            first[..cut].to_owned(),
            Some(ByteCut::FirstLine { bytes: cut }),
        )
    } else {
        (lines[..kept].concat(), Some(ByteCut::Lines { kept }))
    }
}

/// Filter lines by `grep`, matching the model path: an invalid regex leaves
/// the text unfiltered. The script result says so instead of staying silent.
fn grep_lines<'a>(
    lines: Vec<&'a str>,
    params: &AttachmentReadParams,
    notes: &mut Vec<String>,
) -> Vec<&'a str> {
    let Some(pattern) = &params.grep else {
        return lines;
    };
    match fancy_regex::Regex::new(pattern) {
        Ok(re) => {
            let total = lines.len();
            let filtered: Vec<&'a str> = lines
                .into_iter()
                // Match the line without its terminator, like the model
                // path's `lines()` view; emit the original slice.
                .filter(|line| re.is_match(match_view(line)).unwrap_or(false))
                .collect();
            if filtered.len() < total {
                notes.push(format!(
                    "grep left out {} of {} lines",
                    total - filtered.len(),
                    total
                ));
            }
            filtered
        }
        Err(_) => {
            notes.push(format!("invalid regex {pattern:?}; grep not applied"));
            lines
        }
    }
}

/// The line as the model path's `lines()` view shows it: exactly one
/// trailing LF, or one CRLF pair, removed — never a lone CR, which is
/// content, not a terminator. `^foo$` matches `foo\n` and `foo\r\n` the
/// way `text.lines()` matching always has, while an unterminated final
/// `foo\r` matches only `^foo\r$`; output keeps the original bytes.
fn match_view(line: &str) -> &str {
    line.strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'))
        .unwrap_or(line)
}
