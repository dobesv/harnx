---
title: "Custom markdown renderer with widget cache to avoid per-frame O(N) parsing"
date: 2026-05-06
category: "performance-issues"
problem_type: performance_issue
component: "harnx-tui"
root_cause: "tui-markdown dependency parsed markdown on every frame, re-implemented native table support needed custom renderer with efficient caching pattern"
resolution_type: code_fix
severity: high
tags:
  - tui
  - ratatui
  - markdown
  - caching
  - widget-architecture
  - pulldown-cmark
plan_ref: "harnx-markdown-tables-perf"
---

## Problem

The `tui-markdown` crate parsed markdown on every frame, causing O(N) per-frame overhead on long transcripts. It also lacked proper GFM table support. Replacing it with a custom `pulldown-cmark` renderer required solving a fundamental constraint: `ratatui::widgets::Widget::render` consumes `self`, making it impossible to cache widgets directly.

## Symptoms

- CPU usage spikes when scrolling through long transcripts with markdown content
- Lack of table rendering in TUI — tables displayed as raw text
- Each frame re-parsed entire transcript items containing markdown

## Investigation Steps

Analyzed `tui-markdown` source to understand the parsing overhead. Traced frame rendering in harnx-tui to confirm per-frame re-parsing. Explored caching rendered widgets but hit wall: `Widget::render(self, area, buf)` takes ownership. Experimented with storing `Paragraph` and `Table` widgets in cache — compilation failed because widgets can't be cloned and are consumed on render. Designed alternative: cache the *data* needed to build widgets, reconstruct widgets each frame from cached data.

## Root Cause

Two issues:
1. `tui-markdown` parsed markdown on every render call, no caching layer
2. Ratatui's `Widget` trait uses move semantics (`fn render(self, ...)`) — cached widgets would be consumed and unavailable for subsequent frames

## Solution

### 1. Custom Renderer with Owned Data Types

Created `MarkdownBlockData` enum holding owned rendering data (not widgets):

```rust
#[derive(Clone, Debug)]
pub enum MarkdownBlockData {
    Paragraph {
        lines: Vec<Line<'static>>,
        height: u16,
    },
    Table {
        header: Option<Row<'static>>,
        rows: Vec<Row<'static>>,
        col_widths: Vec<usize>,
        height: u16,
    },
}

#[derive(Clone, Debug, Default)]
pub struct RenderedEntry {
    pub blocks: Vec<MarkdownBlockData>,
    pub total_height: u16,
}
```

`RenderedEntry::render()` iterates blocks and builds widgets on demand:

```rust
impl Widget for RenderedEntry {
    fn render(self, area: Rect, buf: &mut Buffer) {
        for block in &self.blocks {
            match block {
                MarkdownBlockData::Paragraph { lines, .. } => {
                    Paragraph::new(lines.clone()).render(area, buf);
                }
                MarkdownBlockData::Table { header, rows, col_widths, .. } => {
                    let table = Table::new(rows.clone(), ...);
                    table.render(area, buf);
                }
            }
        }
    }
}
```

### 2. Width-Keyed Render Cache

Added `rendered_cache: Option<(u16, RenderedEntry)>` to `TranscriptItem` variants:

```rust
TranscriptItem::AssistantText {
    text,
    seq,
    timestamp,
    rendered_cache: None,  // Initially empty
}

// On render:
if let Some((cached_width, entry)) = &item.rendered_cache {
    if *cached_width == width {
        return entry.clone();  // Cache hit
    }
}
// Cache miss: parse and store
let entry = render_markdown(&text, style, width);
item.rendered_cache = Some((width, entry.clone()));
```

Cache invalidates on terminal width change (width key mismatch).

### 3. Clone Requirement for Scroll Widget

`ratatui_widget_scrolling::Scroll` takes `&'a [Element]` and a `Fn(&'a Element) -> (height, widget)`. Since `Widget::render` consumes self, the closure must produce a new widget each call. `RenderedEntry: Clone` enables this:

```rust
let entries: Vec<RenderedEntry> = ...;
scroll.render(frame, area, &entries, |entry| {
    (entry.total_height, entry.clone())
});
```

### 4. Selection Highlighting Across Block Types

Selection style must apply to both `Paragraph` and `Table` blocks within the selected entry:

```rust
for block in &self.blocks {
    match block {
        MarkdownBlockData::Paragraph { lines, height } => {
            let paragraph = Paragraph::new(lines.clone())
                .style(selection_style);  // Apply here
            // ...
        }
        MarkdownBlockData::Table { header, rows, .. } => {
            let table = Table::new(rows.clone(), ...)
                .row_style(selection_style);  // And here
            // ...
        }
    }
}
```

### 5. Metadata Suffix Fallback

All-table messages need metadata suffix appended as separate `Paragraph` block:

```rust
// If message has only tables, append metadata suffix
if is_all_tables && (seq.is_some() || timestamp.is_some()) {
    blocks.push(MarkdownBlockData::Paragraph {
        lines: vec![Line::from(format!("[seq={}, ts={}]", seq, ts))],
        height: 1,
    });
}
```

## Why This Works

**Data vs widget caching:** Storing `MarkdownBlockData` (owned lines, rows, widths) instead of widgets works because:
- Data is `Clone` — cache hands out copies each frame
- Widgets are cheap to build from data — just assembling references
- Width-keyed cache ensures correct reflow on resize

**Clone for scroll widget:** `Scroll` needs elements cloneable because it calls the closure multiple times (height calculation + render). `RenderedEntry: Clone` satisfies this.

**GFM tables native rendering:** `ratatui::widgets::Table` renders tables natively with proper column alignment, borders, and wrapping — no custom drawing code needed.

## Prevention Strategies

**Test cases:**
- Benchmark render time for 1000+ item transcript before/after
- Verify table column widths computed correctly from content
- Test resize invalidates cache and re-renders at new width
- Verify selection style applies to both paragraph and table blocks
- Test all-table messages show metadata suffix

**Best practices:**
- Cache rendering data (`Line`, `Row`, `Span`), not widgets
- Key caches by terminal width to handle resize
- Implement `Clone` on cached data types for multi-consumer patterns
- Use native `ratatui::widgets::Table` for table rendering — don't reinvent
- When `Widget::render` consumes self, store data and build widgets in `render()`

**Code review checklist:**
- [ ] Render cache keyed by width, not just presence?
- [ ] Cached data implements `Clone`?
- [ ] Selection style applied to all block types?
- [ ] Metadata suffix handled for table-only entries?
- [ ] Cache invalidated on width change?

## Related Issues

- **Related Solution:** [logic-errors/tui-browsing-full-transcript-render-2026-05-05.md](../logic-errors/tui-browsing-full-transcript-render-2026-05-05.md) — Scroll state ownership pattern for transcript browsing
- **Commits:**
  - `e3ef3160` feat(tui): add MarkdownBlockData/RenderedEntry widget types
  - `abfa0371` perf(tui): add width-keyed render cache to transcript items
  - `5e2b7c9a` feat(tui,render): add GFM table support to terminal renderer
