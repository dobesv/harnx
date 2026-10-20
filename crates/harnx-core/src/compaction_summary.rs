//! Shared format for the recovery note stored at the end of `Compress.prompt`.
//! Keep persistence and display stripping together so the note cannot drift.

const NOTE_START: &str = "\n\n[Earlier conversation: ";
const COUNT_END: &str = " message(s)";
const RANGE_START: &str = " (log entries ";
const RANGE_SEPARATOR: &str = "–";
const RANGE_END: &str = ")";
const NOTE_END: &str = " were summarized above. \
The full pre-compaction transcript remains in this session's log; use the \
`harnx_agent_session_history_read` tool to search it by entry index, type, tool name, or text.]";

/// Append the runtime recovery note without changing its persisted format.
/// `covered` describes the summarized messages, not the whole archived window.
pub fn append_recovery_note(
    summary: String,
    covered: (Option<usize>, Option<usize>, usize),
) -> String {
    let (from, to, count) = covered;
    let range = match (from, to) {
        (Some(a), Some(b)) => format!("{RANGE_START}{a}{RANGE_SEPARATOR}{b}{RANGE_END}"),
        _ => String::new(),
    };
    format!("{summary}{NOTE_START}{count}{COUNT_END}{range}{NOTE_END}")
}

/// Remove one exact trailing recovery note for display. All other text,
/// including summary whitespace, similar prose and embedded notes, is preserved.
/// The original string remains available to log readers and model context.
pub fn display_summary(prompt: &str) -> &str {
    prompt
        .strip_suffix(NOTE_END)
        .and_then(|head| head.rsplit_once(NOTE_START))
        .filter(|(_, coverage)| valid_coverage(coverage))
        .map_or(prompt, |(summary, _)| summary)
}

fn valid_coverage(coverage: &str) -> bool {
    let Some((count, range)) = coverage.split_once(COUNT_END) else {
        return false;
    };
    canonical_decimal(count) && (range.is_empty() || valid_range(range))
}

fn valid_range(range: &str) -> bool {
    let bounds = range
        .strip_prefix(RANGE_START)
        .and_then(|value| value.strip_suffix(RANGE_END))
        .and_then(|value| value.split_once(RANGE_SEPARATOR));
    bounds.is_some_and(|(from, to)| canonical_decimal(from) && canonical_decimal(to))
}

fn canonical_decimal(value: &str) -> bool {
    // Use u64 so a 32-bit reader can still recognize notes from 64-bit writers.
    value
        .parse::<u64>()
        .is_ok_and(|number| number.to_string() == value)
}

#[cfg(test)]
mod tests;
