//! Shared bounded-read contract for inspecting a sub-agent session's durable
//! log (plan #1550, Task 1). One `read` options object is shared by
//! `session_load` and the async send/wait tools; one deterministic event
//! renderer defines the byte space every continuation and omitted range refers
//! to. The pure functions here operate on a durable, index-annotated window
//! (`(seq, SessionLogEntry)` pairs); loading that window from the log with the
//! same scan caps is the wired tools' job.
//!
//! Invariants frozen here and relied on by later tasks:
//! * The entire serialized read result — events, text, and every metadata
//!   field — fits inside `max_output_bytes`. Impossible budgets are errors,
//!   never silently invalid JSON.
//! * Nothing is silently skipped: preview or budget holes are reported as
//!   `omitted_ranges` (half-open byte intervals of the canonical rendered
//!   text, keyed by durable entry index) and recoverable via the explicit
//!   `entry_index` + `byte_offset` continuation mode.
//! * All text cuts land on UTF-8 character boundaries.

use crate::nats_session_log::serialize_entry;
use crate::session_history::{entry_searchable_text, entry_tool_names, entry_type};
use fancy_regex::Regex;
use harnx_core::session::SessionLogEntry;
use serde::Deserialize;
use serde_json::{json, Value};

mod page;
pub(crate) use page::{read_window_events, EventWindow};

// Physical scan caps live with the physical reader (nats_session_log) so the
// bounded JetStream scan and every in-memory simulation share one source of
// truth. Re-exported here because the read contract refers to them.

/// Default number of matched events per page.
pub(crate) const DEFAULT_READ_LIMIT: u64 = 100;
/// Maximum number of matched events per page.
pub(crate) const MAX_READ_LIMIT: u64 = 500;
/// Default head-preview lines per event.
pub(crate) const DEFAULT_HEAD_LINES: u64 = 25;
/// Default tail-preview lines per event.
pub(crate) const DEFAULT_TAIL_LINES: u64 = 75;
/// Default serialized budget of one read result, including metadata.
pub(crate) const DEFAULT_MAX_OUTPUT_BYTES: u64 = 16_384;
/// Smallest accepted `max_output_bytes`. Keeps any metadata-only page valid.
pub(crate) const MIN_MAX_OUTPUT_BYTES: u64 = 1_024;
/// Largest accepted `max_output_bytes`.
pub(crate) const MAX_MAX_OUTPUT_BYTES: u64 = 65_536;
// Physical scan caps live with the physical reader (nats_session_log) so the
// bounded JetStream scan and every in-memory simulation share one source of
// truth. SCAN_BYTE_LIMIT has no in-page use yet — only the physical reader
// and tests enforce it — so the import allowance comes off when wiring does.
#[allow(unused_imports)]
pub(crate) use crate::nats_session_log::SCAN_BYTE_LIMIT;
pub(crate) use crate::nats_session_log::SCAN_RECORD_LIMIT;

/// Filterable, bounded view of a session's durable log, shared by
/// `session_load` and `wait.read`. All fields optional; `null` is equivalent
/// to omission.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadOptions {
    /// First durable entry index to consider (inclusive).
    #[serde(default)]
    pub(crate) index_min: Option<u64>,
    /// Last durable entry index to consider (inclusive). Fixed index_max
    /// values make historical paging stable against later writes.
    #[serde(default)]
    pub(crate) index_max: Option<u64>,
    /// Canonical entry types to keep (as produced by the history tools).
    #[serde(default)]
    pub(crate) types: Option<Vec<String>>,
    /// Keep only entries referencing this tool name.
    #[serde(default)]
    pub(crate) tool_name: Option<String>,
    /// Keep entries whose searchable text matches this regex.
    #[serde(default)]
    pub(crate) text_regex: Option<String>,
    /// Matched events per page.
    #[serde(default)]
    pub(crate) limit: Option<u64>,
    /// Head-preview lines per event.
    #[serde(default)]
    pub(crate) head_lines: Option<u64>,
    /// Tail-preview lines per event.
    #[serde(default)]
    pub(crate) tail_lines: Option<u64>,
    /// Serialized budget of the whole read result, metadata included.
    #[serde(default)]
    pub(crate) max_output_bytes: Option<u64>,
    /// Continuation selector: pick one durable event. Must be paired with
    /// `byte_offset`, and forbids options that could invalidate offsets.
    #[serde(default)]
    pub(crate) entry_index: Option<u64>,
    /// Zero-based UTF-8 byte offset into the selected event's canonical
    /// rendered text.
    #[serde(default)]
    pub(crate) byte_offset: Option<u64>,
}

/// Sequential (default) read parameters after validation.
#[derive(Debug, Clone)]
pub(crate) struct SequentialRead {
    pub(crate) index_min: Option<u64>,
    pub(crate) index_max: Option<u64>,
    types: Option<Vec<String>>,
    tool_name: Option<String>,
    regex: Option<Regex>,
    pub(crate) limit: usize,
    pub(crate) head_lines: usize,
    pub(crate) tail_lines: usize,
}

/// One of the two read modes. `Sequential` pages whole events; `Continuation`
/// delivers one event's rendered bytes from a validated offset so omitted
/// preview material can be recovered exactly.
#[derive(Debug, Clone)]
pub(crate) enum ReadMode {
    Sequential(SequentialRead),
    Continuation { entry_index: u64, byte_offset: u64 },
}

/// A validated `read` object.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedRead {
    pub(crate) mode: ReadMode,
    pub(crate) max_output_bytes: usize,
}

impl Default for ResolvedRead {
    fn default() -> Self {
        Self {
            mode: ReadMode::Sequential(SequentialRead {
                index_min: None,
                index_max: None,
                types: None,
                tool_name: None,
                regex: None,
                limit: DEFAULT_READ_LIMIT as usize,
                head_lines: DEFAULT_HEAD_LINES as usize,
                tail_lines: DEFAULT_TAIL_LINES as usize,
            }),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES as usize,
        }
    }
}

impl ReadOptions {
    fn resolve_continuation(&self) -> Result<Option<ReadMode>, String> {
        let (entry_index, byte_offset) = match (self.entry_index, self.byte_offset) {
            (None, None) => return Ok(None),
            (Some(entry_index), Some(byte_offset)) => (entry_index, byte_offset),
            _ => {
                return Err(
                    "read.entry_index and read.byte_offset must be provided together".to_string(),
                )
            }
        };
        for (present, option) in [
            (self.types.is_some(), "types"),
            (self.tool_name.is_some(), "tool_name"),
            (self.text_regex.is_some(), "text_regex"),
            (
                self.index_min.or(self.index_max).is_some(),
                "index_min or read.index_max",
            ),
            (
                self.head_lines.or(self.tail_lines).is_some(),
                "head_lines or read.tail_lines",
            ),
        ] {
            if present {
                return Err(format!(
                    "read continuation (entry_index {entry_index}, byte_offset {byte_offset}) \
                     cannot be combined with read.{option}"
                ));
            }
        }
        Ok(Some(ReadMode::Continuation {
            entry_index,
            byte_offset,
        }))
    }

    /// Validate and resolve. Every rule violation returns a stable,
    /// caller-facing error message (tested).
    pub(crate) fn resolve(&self) -> Result<ResolvedRead, String> {
        let max_output_bytes = self.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES) as usize;
        if !(MIN_MAX_OUTPUT_BYTES as usize..=MAX_MAX_OUTPUT_BYTES as usize)
            .contains(&max_output_bytes)
        {
            return Err(format!(
                "read.max_output_bytes must be between {MIN_MAX_OUTPUT_BYTES} and {MAX_MAX_OUTPUT_BYTES}"
            ));
        }
        if let Some(mode) = self.resolve_continuation()? {
            return Ok(ResolvedRead {
                mode,
                max_output_bytes,
            });
        }
        let limit = self.limit.unwrap_or(DEFAULT_READ_LIMIT);
        if limit == 0 || limit > MAX_READ_LIMIT {
            return Err(format!("read.limit must be between 1 and {MAX_READ_LIMIT}"));
        }
        let regex = self
            .text_regex
            .as_deref()
            .map(Regex::new)
            .transpose()
            .map_err(|error| format!("invalid read.text_regex: {error}"))?;
        Ok(ResolvedRead {
            mode: ReadMode::Sequential(SequentialRead {
                index_min: self.index_min,
                index_max: self.index_max,
                types: self.types.clone(),
                tool_name: self.tool_name.clone(),
                regex,
                limit: limit as usize,
                head_lines: self.head_lines.unwrap_or(DEFAULT_HEAD_LINES) as usize,
                tail_lines: self.tail_lines.unwrap_or(DEFAULT_TAIL_LINES) as usize,
            }),
            max_output_bytes,
        })
    }
}

/// The `read` object of the shared schema. Every level rejects unknown
/// fields so nested arguments fail loudly instead of drifting.
pub(crate) fn read_object_schema() -> Value {
    let mut properties = sequential_read_properties();
    properties
        .as_object_mut()
        .expect("schema properties")
        .extend(
            continuation_read_properties()
                .as_object()
                .expect("continuation properties")
                .clone(),
        );
    json!({
        "type": "object",
        "description": "Bounded, filterable view of the sub-agent session's durable log. \
    Returns a page of events plus truncation/continuation metadata; the whole \
    serialized result respects max_output_bytes. Omitted preview material is \
    reported in omitted_ranges and can be recovered with entry_index + \
    byte_offset continuation.",
        "properties": properties,
        "additionalProperties": false
    })
}

fn sequential_read_properties() -> Value {
    json!({
            "index_min": {
                "type": "integer",
                "minimum": 0,
                "description": "First durable entry index to consider (inclusive)."
            },
            "index_max": {
                "type": "integer",
                "minimum": 0,
                "description": "Last durable entry index to consider (inclusive). \
    Pinning index_max makes historical paging stable against later writes."
            },
            "types": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Keep only these canonical entry types (message, \
    tool_calls, tool_results, cancel, error, turn_end, sub_agent_started, ...)."
            },
            "tool_name": {
                "type": "string",
                "description": "Keep only entries referencing this tool name."
            },
            "text_regex": {
                "type": "string",
                "description": "Keep entries whose searchable text matches this \
    regular expression. Invalid regexes are rejected before any read happens."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 500,
                "description": "Maximum number of matched events per page. Default 100."
            },
            "head_lines": {
                "type": "integer",
                "minimum": 0,
                "description": "Head-preview lines per event when its rendered text \
    exceeds the line window. Default 25."
            },
            "tail_lines": {
                "type": "integer",
                "minimum": 0,
                "description": "Tail-preview lines per event when its rendered text \
    exceeds the line window. Default 75."
            },
            "max_output_bytes": {
                "type": "integer",
                "minimum": 1024,
                "maximum": 65536,
                "description": "Serialized budget of the whole read result, \
    metadata included. Default 16384."
            }
    })
}

fn continuation_read_properties() -> Value {
    json!({
            "entry_index": {
                "type": "integer",
                "minimum": 0,
                "description": "Continuation: select this one durable event for \
    byte-range delivery. Must be paired with byte_offset and cannot be combined \
    with filters, index_min/index_max or head_lines/tail_lines."
            },
            "byte_offset": {
                "type": "integer",
                "minimum": 0,
                "description": "Zero-based UTF-8 byte offset into the selected \
    event's canonical rendered text. Off-range or mid-character offsets are \
    rejected."
            }
    })
}

/// Deterministic canonical rendering of one durable entry. Continuation
/// offsets and omitted ranges refer to exactly this text — never raw broker
/// bytes or escaped serialized JSON. Falls back to a stub if YAML rendering
/// itself fails (a bug, not an input condition), so rendering is total.
pub(crate) fn render_event_text(entry: &SessionLogEntry) -> String {
    serialize_entry(entry)
        .unwrap_or_else(|_| format!("{{unrenderable {} entry}}", entry_type(entry)))
}

fn entry_matches(read: &SequentialRead, index: u64, entry: &SessionLogEntry) -> bool {
    if read.index_min.is_some_and(|min| index < min) {
        return false;
    }
    if read.index_max.is_some_and(|max| index > max) {
        return false;
    }
    if let Some(types) = &read.types {
        if !types.iter().any(|kind| kind == entry_type(entry)) {
            return false;
        }
    }
    if read
        .tool_name
        .as_ref()
        .is_some_and(|name| !entry_tool_names(entry).iter().any(|n| n == name))
    {
        return false;
    }
    if read
        .regex
        .as_ref()
        .is_some_and(|re| !re.is_match(&entry_searchable_text(entry)).unwrap_or(false))
    {
        return false;
    }
    true
}

/// serde_json string content length of `c` (without quotes), matching
/// serde_json's escape table exactly. Parity is asserted in tests so this
/// can never drift from the serializer that produces the real payload.
fn escaped_char_len(c: char) -> usize {
    match c {
        '"' | '\\' => 2,
        '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Longest prefix of `text` whose serde_json string-content length fits in
/// `max_json_len`. Returns the byte offset (a char boundary) where delivery
/// must stop and the exact JSON length used.
fn fit_text_prefix(text: &str, max_json_len: usize) -> (usize, usize) {
    let mut used = 0usize;
    for (pos, c) in text.char_indices() {
        let cost = escaped_char_len(c);
        if used + cost > max_json_len {
            return (pos, used);
        }
        used += cost;
    }
    (text.len(), used)
}

/// Per-event share of the page budget, in JSON string-content bytes. At the
/// 1024-byte minimum budget this still leaves the row overhead, metadata and
/// at least one sibling row, so the budget-fit loop below never has to drop
/// the first matched event and skip content it promised to report.
fn per_event_text_budget(max_output_bytes: usize) -> usize {
    (max_output_bytes / 4).max(64)
}

/// Canonical byte ranges kept by head/tail line preview. Lines are '\n'
/// separated; a trailing line without newline counts as a line.
fn preview_segments(text: &str, head_lines: usize, tail_lines: usize) -> Vec<(usize, usize)> {
    let total = text.len();
    if total == 0 {
        return Vec::new();
    }
    let mut line_starts = vec![0usize];
    for (i, byte) in text.bytes().enumerate() {
        if byte == b'\n' && i + 1 < total {
            line_starts.push(i + 1);
        }
    }
    if line_starts.len() <= head_lines + tail_lines {
        return vec![(0, total)];
    }
    let mut segments = Vec::new();
    if head_lines > 0 {
        segments.push((0, line_starts[head_lines]));
    }
    if tail_lines > 0 {
        segments.push((line_starts[line_starts.len() - tail_lines], total));
    }
    segments
}

/// Half-open byte interval of a rendered event's text: `[start, end)`.
pub(crate) type ByteRange = (usize, usize);

/// Byte intervals of `[0, total)` not covered by the (ordered,
/// non-overlapping) delivered ranges, merged where adjacent.
fn complement(delivered: &[ByteRange], total: usize) -> Vec<ByteRange> {
    let mut ranges = Vec::new();
    let mut cursor = 0usize;
    for (start, end) in delivered {
        if *start > cursor {
            ranges.push((cursor, *start));
        }
        cursor = cursor.max(*end);
    }
    if cursor < total {
        ranges.push((cursor, total));
    }
    ranges
}

/// Preview a single event and cap it to the per-event JSON budget.
/// Returns the delivered canonical byte ranges and the omitted complement.
fn deliver_event(
    text: &str,
    head_lines: usize,
    tail_lines: usize,
    cap: usize,
) -> (Vec<ByteRange>, Vec<ByteRange>) {
    let segments = preview_segments(text, head_lines, tail_lines);
    if segments.is_empty() {
        return (Vec::new(), complement(&[], text.len()));
    }
    let mut delivered = Vec::with_capacity(segments.len());
    let mut used = 0usize;
    for (start, end) in segments {
        if used >= cap {
            break;
        }
        let segment = &text[start..end];
        let (fit, segment_used) = fit_text_prefix(segment, cap - used);
        if fit > 0 {
            delivered.push((start, start + fit));
            used += segment_used;
        }
        if fit < segment.len() {
            break;
        }
    }
    let omitted = complement(&delivered, text.len());
    (delivered, omitted)
}

/// Run a resolved read against a durable, index-annotated window of the
/// session log. `entries` must be ordered by durable index.
#[cfg(test)]
pub(crate) fn read_window(
    entries: &[(u64, SessionLogEntry)],
    read: &ResolvedRead,
) -> Result<Value, String> {
    match &read.mode {
        ReadMode::Sequential(_) => read_events(entries, read),
        ReadMode::Continuation { .. } => read_continuation(entries, read),
    }
}

/// Sequential mode: filter, limit, render and budget-fit a page of events.
/// Even an empty filtered page reports physical scan progress.
#[cfg(test)]
pub(crate) fn read_events(
    entries: &[(u64, SessionLogEntry)],
    read: &ResolvedRead,
) -> Result<Value, String> {
    read_window_events(
        EventWindow {
            entries,
            capped: false,
            scanned_through: None,
        },
        read,
        0,
    )
}

struct ContinuationEntry<'a> {
    entry_index: u64,
    kind: &'static str,
    text: &'a str,
}

impl ContinuationEntry<'_> {
    fn value(&self, range: std::ops::Range<usize>, next_offset: Option<u64>) -> Value {
        json!({
            "entry_index": self.entry_index,
            "type": self.kind,
            "text": &self.text[range],
            "total_bytes": self.text.len(),
            "truncated": next_offset.is_some(),
            "continuation": next_offset.map(|offset| json!({
                "entry_index": self.entry_index,
                "byte_offset": offset,
            })).unwrap_or(Value::Null),
            "omitted_ranges": [],
        })
    }
}

/// Continuation mode: deliver one event's canonical rendered bytes from a
/// validated offset, exactly filling the budget. Out-of-range and
/// mid-character offsets are validation errors. See
/// [`read_window_continuation`] for the variant that accounts for enclosing
/// metadata.
#[cfg(test)]
pub(crate) fn read_continuation(
    entries: &[(u64, SessionLogEntry)],
    read: &ResolvedRead,
) -> Result<Value, String> {
    read_window_continuation(entries, read, 0)
}

/// Continuation mode with an enclosing-metadata reserve (Task 4 foundation):
/// `outer_reserve_bytes` is subtracted before fitting, so the enclosing
/// result as a whole still respects `max_output_bytes`.
pub(crate) fn read_window_continuation(
    entries: &[(u64, SessionLogEntry)],
    read: &ResolvedRead,
    outer_reserve_bytes: usize,
) -> Result<Value, String> {
    let ReadMode::Continuation {
        entry_index,
        byte_offset,
    } = read.mode
    else {
        return Err("read_window_continuation requires a continuation read".to_string());
    };
    let entry = entries
        .iter()
        .find(|(index, _)| *index == entry_index)
        .map(|(_, entry)| entry)
        .ok_or_else(|| {
            format!("read.entry_index {entry_index} is not present in the session log")
        })?;
    let text = render_event_text(entry);
    let total = text.len();
    if byte_offset > total as u64 {
        return Err(format!(
            "read.byte_offset {byte_offset} is out of range for entry {entry_index} (rendered length {total})"
        ));
    }
    let start = byte_offset as usize;
    if !text.is_char_boundary(start) {
        return Err(format!(
            "read.byte_offset {byte_offset} is not on a UTF-8 character boundary in entry {entry_index}"
        ));
    }
    // Worst-case metadata (continuation present with a maximal offset) is
    // reserved before fitting text so the delivered slice can never push the
    // serialized result over budget.
    let source = ContinuationEntry {
        entry_index,
        kind: entry_type(entry),
        text: &text,
    };
    let skeleton = source.value(0..0, Some(total as u64));
    let skeleton_len = serde_json::to_string(&skeleton)
        .expect("continuation skeleton serializes")
        .len();
    let budget = effective_budget(read.max_output_bytes, outer_reserve_bytes)?;
    let text_budget = budget
        .checked_sub(skeleton_len)
        .ok_or_else(|| format!("read result metadata exceeds max_output_bytes budget {budget}"))?;
    let mut used = 0usize;
    let mut end = start;
    for (pos, c) in text[start..].char_indices() {
        let cost = escaped_char_len(c);
        if used + cost > text_budget {
            break;
        }
        used += cost;
        end = start + pos + c.len_utf8();
    }
    let truncated = end < total;
    Ok(source.value(start..end, truncated.then_some(end as u64)))
}

/// Serial bytes left after reserving enclosing metadata. A reserve that
/// leaves nothing (or more) for the page is a validation error, not silently
/// invalid JSON: the caller must reject the request before any side effect.
pub(crate) fn effective_budget(
    max_output_bytes: usize,
    outer_reserve_bytes: usize,
) -> Result<usize, String> {
    if outer_reserve_bytes >= max_output_bytes {
        return Err(format!(
            "enclosing metadata reserve ({outer_reserve_bytes} bytes) leaves no \
             max_output_bytes budget for the read result ({max_output_bytes} bytes)"
        ));
    }
    Ok(max_output_bytes - outer_reserve_bytes)
}

/// Clamp a resolved read to a prompt's durable turn bounds (Task 4b load
/// wiring). Sequential reads cannot escape the turn; a continuation read
/// must already point inside it, or the caller could page outside the
/// prompt's scope after the bounds were validated.
pub(crate) fn scope_read_to_prompt(
    read: &mut ResolvedRead,
    start: u64,
    end: u64,
) -> Result<(), String> {
    match &mut read.mode {
        ReadMode::Sequential(seq) => {
            seq.index_min = Some(seq.index_min.unwrap_or(start).max(start));
            seq.index_max = Some(seq.index_max.unwrap_or(end).min(end));
        }
        ReadMode::Continuation { entry_index, .. }
            if *entry_index < start || *entry_index > end =>
        {
            return Err("entry_outside_prompt_scope".to_string());
        }
        _ => {}
    }
    Ok(())
}

/// Shared backend seam for the Task 3 observer and the Task 4b session_load
/// wiring: run a resolved read against the durable NATS session log with the
/// physical scan caps. Pure read — never appends, activates or repairs; a
/// missing log reads as an empty page. `outer_reserve_bytes` is the
/// serialized size of the metadata the caller will wrap around the returned
/// page, so the whole tool-owned result respects `max_output_bytes`.
pub(crate) async fn read_session_log_window(
    log: &crate::nats_session_log::NatsSessionLog,
    read: &ResolvedRead,
    outer_reserve_bytes: usize,
) -> Result<Value, String> {
    match &read.mode {
        ReadMode::Sequential(seq) => {
            let window = log
                .load_window_async(seq.index_min, seq.index_max)
                .await
                .map_err(|error| format!("read session log: {error:#}"))?;
            read_window_events(
                EventWindow {
                    entries: &window.entries,
                    capped: window.capped,
                    scanned_through: window.scanned_through,
                },
                read,
                outer_reserve_bytes,
            )
        }
        ReadMode::Continuation { entry_index, .. } => {
            let entry = log
                .load_entry_async(*entry_index)
                .await
                .map_err(|error| format!("read session log entry {entry_index}: {error:#}"))?;
            let entries: Vec<(u64, SessionLogEntry)> = entry
                .map(|entry| vec![(*entry_index, entry)])
                .unwrap_or_default();
            read_window_continuation(&entries, read, outer_reserve_bytes)
        }
    }
}

#[cfg(test)]
#[path = "session_read_tests.rs"]
mod session_read_tests;

#[cfg(test)]
#[path = "session_read_window_tests.rs"]
mod session_read_window_tests;
