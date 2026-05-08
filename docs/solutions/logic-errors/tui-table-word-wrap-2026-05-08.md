---
title: "TUI markdown table word wrap with style preservation and multi-line rows"
date: 2026-05-08
category: "logic-errors"
problem_type: logic_error
component: "harnx-tui"
root_cause: "table cells truncated long content, no dynamic word wrap or multi-line row support"
resolution_type: code_fix
severity: medium
tags:
  - tui
  - ratatui
  - markdown
  - tables
  - word-wrap
  - textwrap
plan_ref: "table-word-wrap-492"
---

## Problem

TUI markdown tables truncated long cell content instead of wrapping. Long text in table cells was cut off at column width, making tables unreadable for wide content. The renderer needed dynamic word wrap with multi-line row support while preserving text styling (colors, bold) across line breaks.

## Symptoms

- Long text in markdown table cells truncated at column boundary
- Multi-word content in cells displayed as partial text with "..." or cut off mid-word
- No visual indication that content was longer than displayed
- Tables with long content became unusable for reading documentation, logs, or error messages

## Investigation Steps

Anzed `build_table_block()` and found `Cell` construction happened during parsing before column widths were finalized. Column widths required full table scan + shrink pass. Text wrap needed column width as input — chicken-and-egg problem.

Explored using `textwrap::wrap()` directly on span content but hit problem: `textwrap` operates on plain text, losing ratatui `Span` styles (colors, modifiers). Attempted naive `find()` from position 0 for each wrapped line — failed on repeated substrings (e.g., two spans both containing "aa" would both map to first occurrence).

Reviewed ratatui `Table` widget and discovered: `Table` does NOT auto-size rows to cell content. Must call `Row::new(cells).height(n)` explicitly. Missing this caused multi-line content to clip to single line even after wrapping worked.

Tested column shrink behavior and found hardcoded minimum width of 3 chars was too small for meaningful wrap. Needed higher floor to ensure textwrap had room to work.

## Root Cause

**Deferred construction issue:** Ratatui `Cell` objects must be constructed *after* column widths are known (post-scan + shrink). Original code built cells during parse, before widths finalized.

**Style loss during wrap:** `textwrap::wrap()` returns plain text slices. Mapping back to original styled spans required tracking byte offsets and intersecting ranges carefully.

**Missing row height:** Ratatui `Row` defaults to height=1. Multi-line cells need explicit `Row::height(max_lines)` call.

**Insufficient minimum width:** Hardcoded `min_width=3` allowed columns to shrink too far, preventing effective wrap.

## Solution

### 1. Deferred Cell Construction Pattern

Store raw `Vec<Span>` in `TableState` during parse. Build `Cell` in `build_table_block()` *after* shrink pass:

```rust
// During parsing — store raw spans, don't build Cell yet
struct TableState {
    header: Option<Vec<Vec<Span<'static>>>>,  // Raw spans, not Cell
    rows: Vec<Vec<Vec<Span<'static>>>>,        // Raw spans per row per cell
    // ... width tracking ...
}

// In build_table_block() — after shrink
fn build_table_block(state: TableState, width: u16) -> MarkdownBlockData {
    // First: compute natural widths, then shrink
    shrink_table_widths(&mut col_widths, width, &natural_widths);

    // Now: wrap and build cells using final column widths
    let rows = state.rows.into_iter().map(|row| {
        row.into_iter().enumerate().map(|(index, spans)| {
            let lines = wrap_spans(spans, col_widths[index]);
            Cell::from(Text::from(lines))
        }).collect()
    }).collect();
}
```

### 2. Style-Preserving Text Wrap

```rust
fn wrap_spans(spans: Vec<Span<'static>>, width: u16) -> Vec<Line<'static>> {
    // Concatenate spans to plain text, tracking byte ranges
    let mut plain_text = String::new();
    let mut span_ranges = Vec::new();
    for span in spans {
        let byte_start = plain_text.len();
        plain_text.push_str(span.content.as_ref());
        let byte_end = plain_text.len();
        span_ranges.push((byte_start, byte_end, span.style));
    }

    // Wrap plain text
    let wrapped = textwrap::wrap(&plain_text, width as usize);

    // Reconstruct styled spans per line
    let mut search_start = 0usize;  // Critical: advancing cursor, not find from 0
    let mut lines = Vec::with_capacity(wrapped.len());

    for wrapped_line in wrapped {
        let wrapped_line = wrapped_line.as_ref();
        // Find line position using advancing cursor (handles repeated substrings)
        let relative_start = plain_text[search_start..]
            .find(wrapped_line)
            .unwrap_or_default();
        let line_start = search_start + relative_start;
        let line_end = line_start + wrapped_line.len();
        search_start = line_end;  // Advance cursor

        // Intersect original span ranges with this line's byte range
        let line_spans = span_ranges.iter()
            .filter_map(|(span_start, span_end, style)| {
                let overlap_start = (*span_start).max(line_start);
                let overlap_end = (*span_end).min(line_end);
                if overlap_start < overlap_end {
                    Some(Span::styled(
                        plain_text[overlap_start..overlap_end].to_string(),
                        *style,
                    ))
                } else {
                    None
                }
            })
            .collect();
        lines.push(Line::from(line_spans));
    }
    lines
}
```

### 3. Explicit Row Height

```rust
// In Widget::render for Table
for (row_cells, row_height) in rows.iter().zip(row_heights.iter()) {
    let row = Row::new(row_cells.clone())
        .height(*row_height);  // MUST set explicitly
    table = table.row(row);
}
```

### 4. Height Formula for Wrapped Tables

```rust
let height = 2  // top + bottom borders
    + if header.is_some() {
        header_height + 1  // header rows + separator line
    } else {
        0
    }
    + row_heights.iter().copied().sum::<u16>();  // sum of all body row heights
```

Note: `header_height` can be >1 if header cells wrap. The `+1` for separator is distinct from header height.

### 5. Column Minimum-Width Policy

```rust
fn shrink_table_widths(col_widths: &mut [u16], available_width: u16, natural_widths: &[u16]) {
    let min_widths: Vec<u16> = natural_widths.iter()
        .map(|width| if *width > 10 { 10 } else { *width })
        .collect();
    // ... rest of shrink logic uses min_widths as floor
}
```

Short columns (≤10 chars natural width) shrink to fit content. Wide columns cannot shrink below 10 chars, ensuring wrap has room.

## Why This Works

**Deferred construction:** Building cells after shrink ensures `wrap_spans()` receives correct final column width. Required for any layout that needs post-processing with width-dependent logic.

**Advancing cursor:** `search_start` tracks position in `plain_text`. Each wrapped line search starts from previous line end, not from 0. Handles repeated substrings correctly (test: `"aa"` styled Red + `"aa"` styled Blue produces two distinct styled spans).

**Byte range intersection:** Original spans may span multiple wrapped lines. Intersection logic handles:
- Span entirely within line: included with original style
- Span split across lines: each line gets its slice with same style
- Multiple spans per line: all reconstructed with correct styles

**Explicit height:** Ratatui's `Row::height()` controls how many lines the row occupies. Multi-line cells without this setting clip to first line.

**Sensible minimum:** `min(natural_width, 10)` balances narrow columns (use exact content width) vs wide columns (reserve space for wrap).

## Prevention Strategies

**Test cases:**
- Wrap spans with repeated substrings — verify distinct styles preserved
- Mid-span wrap — single span splitting across >1 line
- Unicode multibyte characters — ensure byte indexing doesn't panic
- Explicit `\n` in cell content — verify hard line breaks
- Empty spans, zero width, single char edge cases
- Table height formula — header, body rows, separator, borders

**Best practices:**
- Defer construction of layout-dependent widgets until widths finalized
- Use advancing cursor (not `find()` from 0) when mapping positions back to source
- Track byte offsets when preserving styles across text transformations
- Always set `Row::height()` explicitly for multi-line content in ratatui tables
- Use `saturating_add` for u16 height sums to avoid overflow panics

**Code review checklist:**
- [ ] Cell construction deferred until after width computation?
- [ ] wrap_spans uses advancing cursor, not find from position 0?
- [ ] Row height explicitly set for multi-line cells?
- [ ] Height formula accounts for header separator (+1)?
- [ ] Minimum column width allows meaningful wrap (>3 chars)?

## Related Issues

- **Prior Art:** [performance-issues/tui-markdown-widget-architecture-2026-05-06.md](../performance-issues/tui-markdown-widget-architecture-2026-05-06.md) — Original markdown renderer architecture with data caching pattern
- **GitHub Issue:** #492 — TUI markdown table word wrap
- **Key Functions:**
  - `wrap_spans()` — style-preserving text wrap with byte offset remap
  - `build_table_block()` — deferred Cell construction after shrink
  - `shrink_table_widths()` — column width shrink with minimum floor policy
