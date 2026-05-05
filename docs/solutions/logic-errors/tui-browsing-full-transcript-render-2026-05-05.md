---
title: "TUI browsing mode full transcript render and scroll state ownership"
date: 2026-05-05
category: "logic-errors"
problem_type: logic_error
component: "harnx-tui"
root_cause: "browsing view rendered single-item detail panel instead of full transcript, and scroll state was reset per keypress instead of managed by renderer"
resolution_type: code_fix
severity: medium
tags:
  - tui
  - overlay
  - scroll-state
  - ratatui
  - navigation
  - rendering
plan_ref: "harnx-tui-browsing-mode-transcript-view"
---

## Problem

Browsing mode in harnx-tui had two UX bugs: (1) keyboard shortcuts were not visible because the view rendered only a single focused item in a bordered "History Browser" panel, obscuring full transcript context, and (2) arrow navigation moved line-by-line instead of item-by-item because scroll state was reset to a fresh `ScrollState` on every keypress, fighting the centering logic.

## Symptoms

- Browsing mode showed a narrow bordered panel with one item detail, not the full transcript list
- Keyboard shortcuts footer was rendered but the transcript list was hidden behind the detail panel
- Arrow Up/Down in browsing mode would sometimes lose position or scroll unexpectedly
- Focused item highlighting wasn't visible across multiple items
- GitHub #454: keyboard shortcuts not visible during history browse
- GitHub #431: arrow navigation was line-by-line instead of item-by-item

## Investigation Steps

1. Reviewed `render_browsing_view()` — found it called `render_entry_detail()` inside a `Block::default().borders(Borders::ALL).title("History Browser")` panel
2. Compared with normal `draw()` — realized normal view iterates all transcript items with `Modifier::REVERSED` for focused item
3. Traced `handle_up_key()` and `handle_down_key()` — discovered `browsing_view_scroll` was reset to fresh `ScrollState::new()` on every keypress
4. Found `scroll_to_focused_item` flag was set in input handler but reset logic interfered
5. Understood the pattern: flag signals "center on focus next render", but resetting scroll state to position 0 defeated the centering

## Root Cause

**Single-item detail panel:** `render_browsing_view()` was designed as a "History Browser" panel showing one item via `render_entry_detail()`, similar to the detail view pattern. This was inappropriate for browsing mode, which should show the full transcript with focus highlighting — an overlay that replaces the screen, not a narrow inspection panel.

**Per-keypress scroll reset:** The input handlers for Up/Down in browsing mode reset `browsing_view_scroll` to a fresh `ScrollState` on every keypress:

```rust
self.app.browsing_view_scroll = {
    let mut s = ratatui_widget_scrolling::ScrollState::new();
    s.follow = false;
    s
};
```

This fought the centering logic. The `scroll_to_focused_item` flag said "center on focused item", but the reset said "start from position 0". The flag and the reset were in conflict.

**Principle violated:** When a flag signals "do X on next render", input handlers should only set the flag — the renderer owns the state mutation. Mutating state in the input path creates conflicting sources of truth.

## Solution

### 1. Replace detail panel with full transcript rendering

`render_browsing_view()` now mirrors `draw()`: iterates all transcript items, applies `Modifier::REVERSED` to the focused item, uses the same `ScrollState.render()` + `Paragraph::wrap()` closure pattern.

Removed the bordered "History Browser" block and `render_entry_detail()` usage.

```rust
// Before: bordered panel showing single item
let block = Block::default()
    .borders(Borders::ALL)
    .title("History Browser");
let inner_area = block.inner(chunks[0]);
frame.render_widget(block, chunks[0]);

let entries_as_vec = /* single focused item */;
self.app.browsing_view_scroll.render(frame, inner_area, &entries_as_vec, |lines| { ... });

// After: full transcript with focus highlight
let transcript_entries: Vec<Vec<Line>> = self.app.transcript
    .iter()
    .enumerate()
    .map(|(i, entry)| {
        let mut lines = Self::render_entry(entry, show_seq, show_ts, use_utc);
        if let Some(range) = &selected_range {
            if range.contains(&i) {
                for line in &mut lines {
                    line.style = line.style.add_modifier(Modifier::REVERSED);
                }
            }
        }
        lines
    })
    .collect();

self.app.browsing_view_scroll.render(frame, chunks[0], &transcript_entries, |lines| {
    let paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
    let height = paragraph.line_count(chunks[0].width);
    (height, paragraph)
});
```

### 2. Remove scroll state reset from input handlers

Removed the per-keypress `browsing_view_scroll` reset from `handle_up_key()` and `handle_down_key()`. The input handler only sets `scroll_to_focused_item = true`. The renderer calls `scroll_position_to_show_item()` to compute the correct position — that's the single source of truth.

```rust
// Input handler: only set the flag
if let Some(prev) = self.find_prev_navigable(focus) {
    self.app.transcript_focus = Some(prev);
    self.app.transcript_browsing = true;
    self.app.scroll_to_focused_item = true;
    // NO scroll state reset here
}

// Renderer: owns the position calculation
if self.app.scroll_to_focused_item {
    if let Some(focus) = self.app.transcript_focus {
        let position = self.app.browsing_view_scroll.scroll_position_to_show_item(
            focus,
            chunks[0].width,
            chunks[0].height as usize,
            self.app.transcript.len(),
        );
        self.app.browsing_view_scroll.position = position;
    }
    self.app.scroll_to_focused_item = false;
}
```

## Why This Works

**Full transcript rendering:** Browsing mode is an overlay that replaces the entire screen. It should render the same content as the normal view but with focus highlighting — not a separate narrow panel. This ensures keyboard shortcuts and other UI elements remain visible in the footer, and the user sees the full context around the focused item.

**Flag ownership pattern:** The `scroll_to_focused_item` flag is a request from the input handler to the renderer: "on next render, center on the focused item". The renderer computes the position via `scroll_position_to_show_item()` and clears the flag. The input handler never touches scroll position directly. This prevents conflicting mutations and ensures the renderer is the single source of truth for scroll state.

**No position reset:** By removing the fresh `ScrollState` on each keypress, the scroll position persists across navigation, and the centering logic works correctly. The `follow = false` setting is preserved from initial setup (not reset each time), maintaining the "don't auto-follow new content" behavior appropriate for history browsing.

## Prevention Strategies

**Test cases:**
- Test browsing mode renders full transcript list with focus highlighting
- Test arrow Up/Down moves focus item-by-item, not line-by-line
- Test focused item stays centered after navigation
- Test keyboard shortcuts visible in footer during browsing mode
- Test scroll position persists across multiple arrow keypresses

**Best practices:**
- When an overlay replaces the full screen, render the same content as the main view with focus/style modifications — not a separate specialized panel
- Use flags to signal "do X on next render" — the renderer owns the state mutation
- Never reset scroll state in input handlers; let the renderer manage position via flags
- Preserve scroll state across navigation events; don't create fresh instances per keypress

**Code review checklist:**
- [ ] Does browsing overlay render full content with focus highlight, not a narrow panel?
- [ ] Is scroll state reset removed from input handlers?
- [ ] Does renderer own scroll position calculation via flags?
- [ ] Are flags cleared by renderer after state mutation?

## Related Issues

- **GitHub:** [#454](https://github.com/dobesv/harnx/issues/454) — Keyboard shortcuts not visible during history browse
- **GitHub:** [#431](https://github.com/dobesv/harnx/issues/431) — Arrow navigation line-by-line vs item-by-item
- **Related Solution:** [logic-errors/tui-browsing-overlay-and-detail-shortcuts-2026-05-04.md](./tui-browsing-overlay-and-detail-shortcuts-2026-05-04.md) — Modal rendering order and input guards for detail/browsing overlays
- **Related Solution:** [logic-errors/tui-exclusive-overlay-pattern-2026-05-02.md](./tui-exclusive-overlay-pattern-2026-05-02.md) — Base overlay pattern with input isolation
