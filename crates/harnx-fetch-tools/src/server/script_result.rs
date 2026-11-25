//! Page results shaped for a bash script: the text in a structured field and
//! notes about cut-off output apart from it. The model-facing results stay in
//! `handler.rs`.

use harnx_core::safety::TruncateOpts;
use harnx_toolset::line_cap_notes;
use harnx_toolset_server::content::WithAudience;
use rmcp::model::{CallToolResult, ContentBlock, Role};
use serde_json::json;

use crate::client::FetchedPage;

use super::FetchParams;

/// Who a result is rendered for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResultStyle {
    /// Text written for a model to read, with in-band truncation notices.
    #[default]
    Model,
    /// Plain page text in `structuredContent`, for a script to pipe.
    Script,
}

/// Build the script-style result for a page whose transformed text is `full`.
/// `json` marks text that is meant to parse as JSON.
pub(super) fn script_page(
    full: &str,
    page: &FetchedPage,
    params: &FetchParams,
    json: bool,
) -> CallToolResult {
    let (window, cut) = params.compatibility_window(full);
    let mut cap_notes = Vec::new();
    let (text, bytes_cut) = keep_whole_lines(&window, byte_cap(params), &mut cap_notes);
    let mut notes: Vec<String> = remaining_note(full, &text, params).into_iter().collect();
    notes.extend(cap_notes);
    notes.extend(line_cap_notes(params.head_lines, params.tail_lines));
    if json && (cut || bytes_cut) {
        notes.push("output is a cut JSON preview, not valid JSON".to_owned());
    }
    let content_type = page
        .content_type
        .as_ref()
        .and_then(|value| value.to_str().ok());
    let fields = json!({
        "text": text,
        "notes": notes,
        "url": params.url,
        "final_url": page.final_url.as_str(),
        "content_type": content_type,
    });
    let summary = format!("Fetched {} ({} bytes)", page.final_url, text.len());
    let mut result = CallToolResult::success(vec![
        ContentBlock::text(summary).with_audience(vec![Role::User])
    ]);
    result.structured_content = Some(fields);
    result
}

fn byte_cap(params: &FetchParams) -> usize {
    params
        .max_output_bytes
        .unwrap_or(TruncateOpts::default().max_output_bytes)
}

/// The note telling a script where to continue, when text remains after what
/// was printed. `printed` is the text after both the character window and the
/// byte cap, so the index follows what the script received, not the window.
fn remaining_note(full: &str, printed: &str, params: &FetchParams) -> Option<String> {
    let total = full.chars().count();
    let next = params.start_index.unwrap_or(0).min(total) + printed.chars().count();
    (next < total).then(|| {
        format!(
            "{} more characters. Use start_index={next} to continue",
            total - next
        )
    })
}

/// Keep whole lines from the start that fit in `cap` bytes. A first line that
/// does not fit alone is cut at a character boundary, so the text never
/// exceeds the cap. Returns the text and whether anything was dropped.
fn keep_whole_lines(window: &str, cap: usize, notes: &mut Vec<String>) -> (String, bool) {
    if window.len() <= cap {
        return (window.to_owned(), false);
    }
    let line_end = |(at, _): (usize, &str)| at + 1;
    let mut kept = window
        .match_indices('\n')
        .map(line_end)
        .take_while(|&end| end <= cap)
        .last()
        .unwrap_or(0);
    if kept == 0 {
        kept = window
            .char_indices()
            .map(|(at, c)| at + c.len_utf8())
            .take_while(|&end| end <= cap)
            .last()
            .unwrap_or(0);
    }
    notes.push(format!(
        "cut at {kept} of {} bytes. Use max_output_bytes to see more",
        window.len()
    ));
    (window[..kept].to_owned(), true)
}

#[cfg(test)]
mod tests;
