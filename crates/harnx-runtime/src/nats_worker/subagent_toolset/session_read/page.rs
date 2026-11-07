//! Sequential page selection, row rendering and whole-result budget fitting.
use super::{
    deliver_event, effective_budget, entry_matches, entry_type, per_event_text_budget,
    render_event_text, ReadMode, ResolvedRead, SequentialRead, SCAN_RECORD_LIMIT,
};
use harnx_core::session::SessionLogEntry;
use serde_json::{json, Value};

/// Entries and physical scan progress belong to the same durable window.
/// Entries must be ordered by durable index; progress can include purged gaps.
pub(crate) struct EventWindow<'a> {
    pub(crate) entries: &'a [(u64, SessionLogEntry)],
    /// The physical scan stopped before the requested range ended.
    pub(crate) capped: bool,
    /// Highest examined sequence, including confirmed gaps after the last entry.
    pub(crate) scanned_through: Option<u64>,
}

impl<'a> EventWindow<'a> {
    fn bounded(self, read: &SequentialRead) -> Self {
        let lower = read.index_min.map_or(0, |min| {
            self.entries.partition_point(|(index, _)| *index < min)
        });
        let upper = read.index_max.map_or(self.entries.len(), |max| {
            lower + self.entries[lower..].partition_point(|(index, _)| *index <= max)
        });
        let in_range = &self.entries[lower..upper];
        let entries = &in_range[..in_range.len().min(SCAN_RECORD_LIMIT)];
        Self {
            entries,
            capped: self.capped || in_range.len() > SCAN_RECORD_LIMIT,
            // A scan that ended on purged sequences must retain that progress,
            // even when no surviving entry matched the filter.
            scanned_through: self
                .scanned_through
                .or_else(|| entries.last().map(|(index, _)| *index)),
        }
    }
}

struct PageSelection<'a> {
    matched: Vec<(u64, &'a SessionLogEntry)>,
    effective_index_max: Option<u64>,
    scanned_through: Option<u64>,
    scan_capped: bool,
}

impl<'a> PageSelection<'a> {
    fn new(window: EventWindow<'a>, read: &SequentialRead) -> Self {
        let window = window.bounded(read);
        let matched = window
            .entries
            .iter()
            .filter(|(index, entry)| entry_matches(read, *index, entry))
            .map(|(index, entry)| (*index, entry))
            .collect();
        let effective_index_max = match (read.index_max, window.scanned_through) {
            (Some(requested), Some(scanned)) => Some(requested.min(scanned)),
            (Some(requested), None) => Some(requested),
            (None, scanned) => scanned,
        };
        Self {
            matched,
            effective_index_max,
            scanned_through: window.scanned_through,
            scan_capped: window.capped,
        }
    }

    fn shape(&self, rows_len: usize) -> PageShape {
        let next_index = if rows_len < self.matched.len() {
            Some(self.matched[rows_len].0)
        } else if self.scan_capped {
            self.scanned_through.map(|index| index + 1)
        } else {
            None
        };
        PageShape {
            next_index,
            effective_index_max: self.effective_index_max,
            scanned_through: self.scanned_through,
            undelivered: self.matched.len() > rows_len,
            scan_capped: self.scan_capped,
        }
    }
}

/// The fit loop and final page must measure byte-identical metadata.
struct PageShape {
    next_index: Option<u64>,
    effective_index_max: Option<u64>,
    scanned_through: Option<u64>,
    undelivered: bool,
    scan_capped: bool,
}

struct RenderedRow {
    value: Value,
    complete: bool,
    omitted: Vec<Value>,
}

impl RenderedRow {
    fn new(index: u64, entry: &SessionLogEntry, read: &SequentialRead, cap: usize) -> Self {
        let text = render_event_text(entry);
        let (delivered, omitted) = deliver_event(&text, read.head_lines, read.tail_lines, cap);
        let rendered: String = delivered
            .iter()
            .map(|(start, end)| &text[*start..*end])
            .collect();
        let complete = omitted.is_empty();
        let omitted = omitted
            .iter()
            .map(|(start, end)| {
                json!({
                    "entry_index": index,
                    "start": start,
                    "end": end,
                })
            })
            .collect();
        Self {
            value: json!({"index": index, "type": entry_type(entry), "text": rendered, "complete": complete}),
            complete,
            omitted,
        }
    }
}

struct PageRows {
    events: Vec<Value>,
    complete: Vec<bool>,
    omitted_ranges: Vec<Value>,
    // Tail removal must drop each row's omission metadata in lockstep.
    omitted_counts: Vec<usize>,
}

impl PageRows {
    fn render(selection: &PageSelection<'_>, read: &SequentialRead, budget: usize) -> Self {
        let deliverable = selection.matched.len().min(read.limit);
        let cap = per_event_text_budget(budget);
        let mut rows = Self {
            events: Vec::with_capacity(deliverable),
            complete: Vec::with_capacity(deliverable),
            omitted_ranges: Vec::new(),
            omitted_counts: Vec::with_capacity(deliverable),
        };
        for (index, entry) in selection.matched.iter().take(deliverable) {
            let row = RenderedRow::new(*index, entry, read, cap);
            rows.omitted_counts.push(row.omitted.len());
            rows.omitted_ranges.extend(row.omitted);
            rows.events.push(row.value);
            rows.complete.push(row.complete);
        }
        rows
    }

    fn value(&self, shape: &PageShape) -> Value {
        let truncated = shape.undelivered
            || shape.scan_capped
            || self.complete.iter().any(|complete| !*complete);
        json!({
            "events": self.events,
            "truncated": truncated,
            "next_index": shape.next_index,
            "effective_index_max": shape.effective_index_max,
            "scanned_through": shape.scanned_through,
            "continuation": null,
            "omitted_ranges": self.omitted_ranges,
        })
    }

    fn pop(&mut self) -> Option<()> {
        self.events.pop()?;
        self.complete.pop();
        if let Some(count) = self.omitted_counts.pop() {
            let keep = self.omitted_ranges.len() - count;
            self.omitted_ranges.truncate(keep);
        }
        Some(())
    }

    fn fit(mut self, selection: &PageSelection<'_>, budget: usize) -> Result<Value, String> {
        let mut value = self.value(&selection.shape(self.events.len()));
        // Recompute next_index after each tail removal so budget cuts cannot
        // skip a matched event or retain omission ranges for an absent row.
        while serde_json::to_string(&value)
            .expect("read page serializes")
            .len()
            > budget
        {
            self.pop().ok_or_else(|| {
                format!("read result metadata exceeds max_output_bytes budget {budget}")
            })?;
            value = self.value(&selection.shape(self.events.len()));
        }
        Ok(value)
    }
}

/// Filter and budget-fit one physical window. The reserve covers metadata
/// added around this page; impossible reserves fail before selection/rendering.
pub(crate) fn read_window_events(
    window: EventWindow<'_>,
    read: &ResolvedRead,
    outer_reserve_bytes: usize,
) -> Result<Value, String> {
    let ReadMode::Sequential(seq) = &read.mode else {
        return Err("read_window_events requires a sequential read".to_string());
    };
    let budget = effective_budget(read.max_output_bytes, outer_reserve_bytes)?;
    let selection = PageSelection::new(window, seq);
    PageRows::render(&selection, seq, budget).fit(&selection, budget)
}
