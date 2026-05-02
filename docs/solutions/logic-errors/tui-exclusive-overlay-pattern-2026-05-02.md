---
title: "TUI exclusive overlay pattern: input isolation and scroll state rendering"
date: 2026-05-02
category: "logic-errors"
problem_type: logic_error
component: "harnx-tui"
root_cause: "multiple independent input paths allowed background mutations during fullscreen overlay"
resolution_type: code_fix
severity: high
tags:
  - tui
  - overlay
  - input-isolation
  - ratatui
  - scroll-state
  - modal
plan_ref: "430-transcript-detail-view"
---

## Problem

Fullscreen overlay in a Ratatui TUI allowed keyboard events to "bleed through" to background handlers, triggering unintended mutations (delete, edit, rewind) while user was in read-only detail view. Additionally, scroll state clamping was needed to prevent dead zones at content boundaries.

## Symptoms

- Pressing `d`, `e`, `r`, `i`, `c` while detail view open triggered delete/edit/rewind/insert/copy on focused transcript item behind overlay
- Rewind/delete could open confirmation modal that remained invisible (rendered behind early return)
- Scrolling to bottom of detail view could leave position beyond last_max_position, creating unresponsive scroll state

## Investigation Steps

1. Traced `handle_key` in `input.rs` — found modal guard at top (`if self.app.modal.is_some()`) correctly isolated modal input
2. Discovered no equivalent guard for `detail_view_open` — key events fell through to transcript action handlers at lines 219-235
3. Reviewed render path — `draw()` performed full main-view rendering before checking `detail_view_open` and calling `Clear` + early return
4. Identified scroll state pattern: `ScrollState::render()` takes `frame, area, &Vec<Vec<Line>>` and closure, then `last_max_position` must be used to clamp `position`

## Root Cause

**Input bleed:** Key handlers for transcript actions (`e`, `d`, `i`, `c`, `r`) only checked `transcript_focus.is_some()`. Detail view left focus intact, so these shortcuts remained active. The pattern used for modal isolation (early return at top of `handle_key`) was not applied to the new overlay.

**Scroll dead zone:** After `ScrollState::render()`, the `position` field could exceed `last_max_position` if content height changed or user scrolled aggressively. Without clamping, subsequent scroll operations would behave unexpectedly.

## Solution

### 1. Exclusive input isolation with top-level guard

Place a single guard at the top of `handle_key`, immediately after modal check:

```rust
// In handle_key(), after modal check
if self.app.detail_view_open {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, KeyModifiers::NONE) => {
            self.app.detail_view_open = false;
        }
        (KeyCode::Up, KeyModifiers::NONE) => {
            self.app.detail_view_scroll.scroll_up();
        }
        (KeyCode::Down, KeyModifiers::NONE) => {
            self.app.detail_view_scroll.scroll_down();
        }
        (KeyCode::PageUp, KeyModifiers::NONE) => {
            for _ in 0..10 {
                self.app.detail_view_scroll.scroll_up();
            }
        }
        (KeyCode::PageDown, KeyModifiers::NONE) => {
            for _ in 0..10 {
                self.app.detail_view_scroll.scroll_down();
            }
        }
        _ => {} // all other keys silently consumed
    }
    return Ok(());
}
```

**Critical:** Use `_ => {}` catch-all that consumes unhandled keys — this prevents bleed-through. Return early after the match block.

### 2. Exclusive render pattern

After rendering fullscreen overlay, return early to skip all other overlays:

```rust
// In draw(), after optional modal rendering
if self.app.detail_view_open {
    frame.render_widget(ratatui::widgets::Clear, size);
    self.render_detail_view(frame, size);
    return;
}
```

The `Clear` widget wipes the full frame before rendering overlay content.

### 3. ScrollState render and clamp pattern

`ScrollState::render()` signature: `render(frame, area, &Vec<Vec<Line>>, |lines| -> (height, Paragraph))`

```rust
self.app.detail_view_scroll.render(
    frame,
    inner_area,
    &entries_as_vec,
    |lines| {
        let paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
        let height = paragraph.line_count(inner_area.width);
        (height, paragraph)
    },
);

// Clamp position to freshly-updated last_max_position
self.app.detail_view_scroll.position = self
    .app
    .detail_view_scroll
    .position
    .min(self.app.detail_view_scroll.last_max_position);
```

The `render()` call updates `last_max_position`; clamping afterward ensures `position` stays in bounds.

## Why This Works

**Top-level guard pattern:** A single early return at the top of `handle_key` (after modal) intercepts all key events while overlay is active. This is cleaner and less error-prone than scattering `&& !self.app.detail_view_open` guards across individual match arms — those are easy to miss when adding new shortcuts.

**Catch-all consumption:** The `_ => {}` arm ensures that keys like `d`, `e`, `r` are silently consumed rather than falling through to background handlers. Users perceive the overlay as truly modal/exclusive.

**Clear + early return rendering:** `Clear` wipes the frame, preventing visual artifacts. Early return avoids unnecessary work (footer, completions, etc.) and ensures no other overlay can render on top.

**Position clamping:** `ScrollState` tracks `position` and `last_max_position` separately. After render updates `last_max_position` based on content height, clamping `position` ensures it never exceeds the scrollable range, preventing "dead zone" where scroll operations have no effect.

## Prevention Strategies

**Test cases:**
- Test that mutation shortcuts (`d`, `e`, `r`, `i`, `c`) are blocked when overlay is open
- Test that navigation keys (arrows, page up/down, esc) work correctly in overlay
- Test that paste events are blocked or handled appropriately
- Test scroll state clamping after content resize

**Best practices:**
- When adding a fullscreen/exclusive overlay, add top-level guard in `handle_key` immediately after modal check
- Use catch-all `_ => {}` to consume unhandled keys — never let them fall through
- Always clamp `ScrollState.position` to `last_max_position` after `render()`
- Order render checks: modal → fullscreen overlay → other overlays → main view

**Code review checklist:**
- [ ] Is overlay input isolation implemented with top-level guard?
- [ ] Does catch-all arm consume unhandled keys?
- [ ] Is `Clear` rendered before overlay content?
- [ ] Does render function return after fullscreen overlay?
- [ ] Is scroll position clamped after `ScrollState::render()`?
- [ ] Are tests added for input isolation behavior, not just flag state?

## Related Issues

- **GitHub:** [#430](https://github.com/dobesv/harnx/issues/430) — Transcript history item detail view
- **Related Solution:** [integration-issues/tui-transcript-focus-navigation-2026-05-01.md](../integration-issues/tui-transcript-focus-navigation-2026-05-01.md) — Focus state patterns for transcript navigation
