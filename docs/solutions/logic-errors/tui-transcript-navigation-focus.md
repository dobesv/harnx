---
title: "TUI transcript keyboard navigation with focus management and auto-scroll"
date: 2026-05-02
category: logic-errors
problem_type: logic_error
component: harnx-tui
root_cause: "missing navigation policy and scroll synchronization"
resolution_type: code_fix
severity: medium
tags:
  - tui
  - keyboard-navigation
  - focus-management
  - scroll-state
plan_ref: "431-433-transcript-nav-fixes"
---

## Problem

Transcript navigation in the harnx TUI lacked a consistent policy for which items can receive keyboard focus, causing arrow-key navigation to land on non-interactive items. Additionally, focus changes did not auto-scroll the viewport to keep the focused item visible, and the `follow` flag (auto-scroll-to-bottom) was not properly managed during focus operations.

## Symptoms

- Arrow-up/down navigation would stop on spacing dividers and non-navigable transcript items
- Focused items could scroll out of view, leaving the user with invisible selection
- `follow` mode would conflict with manual focus navigation, causing unexpected viewport jumps
- No way to view raw tool result content (only markdown-rendered summaries)

## Investigation Steps

1. Identified that all transcript items were treated equally for navigation, but types like `Divider` and `ToolResultList` are visual separators, not actionable content.

2. Traced the Up/Down key handlers and found they simply decremented/incremented the focus index without checking navigability.

3. Found that `scroll_state.follow` was never explicitly set to `false` when focus was gained, allowing the render loop to overwrite manual scroll positions on every frame.

4. Observed that even when `follow` was disabled, there was no mechanism to scroll the viewport to show a newly focused item.

5. Reviewed `ScrollState` and found it tracks `position` (scroll offset from bottom) but had no method to compute what position would make a given item visible.

## Root Cause

Three related issues:

1. **No navigation policy**: The `TranscriptItem` enum had no method to indicate which variants are navigable. Arrow-key handlers used simple index arithmetic without filtering.

2. **Missing scroll-to-focus mechanism**: Focus changes updated `transcript_focus` but never adjusted `scroll_state.position`. The render loop assumed `follow=true` meant "stick to bottom" and `follow=false` meant "hold position", with no third option for "scroll to show focus".

3. **Improper follow flag management**: `follow` was left at its previous value when focus was gained, causing the next render to either snap to bottom or hold an arbitrary position, depending on prior state.

## Solution

### 1. Centralized navigation policy with `is_navigable()`

Added `is_navigable()` method to `TranscriptItem` enum in `types.rs`:

```rust
pub(crate) fn is_navigable(&self) -> bool {
    matches!(
        self,
        TranscriptItem::UserText { .. }
        | TranscriptItem::AssistantText { .. }
        | TranscriptItem::ToolCall { .. }
        | TranscriptItem::ToolResultMarkdown(_)
    )
}
```

### 2. Navigation helpers that skip non-navigable items

Added `find_prev_navigable()` and `find_next_navigable()` helpers in `input.rs`:

```rust
fn find_prev_navigable(&self, start: usize) -> Option<usize> {
    let mut focus = start;
    while focus > 0 {
        focus -= 1;
        if self.app.transcript[focus].is_navigable() {
            return Some(focus);
        }
    }
    None
}

fn find_next_navigable(&self, mut focus: usize) -> Option<usize> {
    while focus + 1 < self.app.transcript.len() {
        focus += 1;
        if self.app.transcript[focus].is_navigable() {
            return Some(focus);
        }
    }
    None
}
```

All arrow-key handlers now use these helpers instead of raw index arithmetic.

### 3. One-shot scroll-to-focus flag

Added `scroll_to_focused_item: bool` field to `App` struct. Input handlers set this flag when changing focus:

```rust
self.app.transcript_focus = Some(prev);
self.app.scroll_state.follow = false;
self.app.scroll_to_focused_item = true;
```

The render loop consumes and clears the flag:

```rust
if self.app.scroll_to_focused_item {
    if let Some(focus) = self.app.transcript_focus {
        let position = self.app.scroll_state.scroll_position_to_show_item(
            focus,
            chunks[0].width,
            chunks[0].height as usize,
            self.app.transcript.len(),
        );
        self.app.scroll_state.position = position;
    }
    self.app.scroll_to_focused_item = false;
}
```

This pattern avoids overwriting manual scrolling on every frame while still enabling programmatic scroll-to-focus on demand.

### 4. `scroll_position_to_show_item()` on `ScrollState`

Added method to `ScrollState` that computes the `position` value needed to center a given item in the viewport:

```rust
pub fn scroll_position_to_show_item(
    &mut self,
    item_index: usize,
    viewport_width: u16,
    viewport_height: usize,
    num_elements: usize,
) -> usize {
    let height_log = self.get_height_log_from_cache_for_width(viewport_width, num_elements);

    let top_offset: usize = height_log.iter().take(item_index).sum();
    let item_height = height_log.get(item_index).copied().unwrap_or(1);

    let max_scroll_offset = height_log.iter().sum::<usize>().saturating_sub(viewport_height);
    if max_scroll_offset == 0 {
        return 0; // Everything fits
    }

    let target_scroll_offset = if item_height >= viewport_height {
        top_offset // Item is taller than viewport, align top
    } else {
        top_offset.saturating_sub((viewport_height - item_height) / 2) // Center
    }.min(max_scroll_offset);

    max_scroll_offset.saturating_sub(target_scroll_offset)
}
```

Uses the render height cache for accurate item heights. Falls back to 1-line estimates for uncached items (self-corrects on next frame after cache warms).

### 5. `follow` flag lifecycle

Explicit `follow` management:

- **Focus gain**: Set `follow = false` (user is navigating manually)
- **Esc press**: Set `follow = true` (return to live tail)
- **Submit message**: Set `follow = true` (return to live tail)
- **Enter history preview from top**: Do NOT restore `follow` (different navigation mode)

## Why This Works

- `is_navigable()` centralizes the policy in one place, making it easy to adjust which items are focusable without updating multiple handlers.

- One-shot `scroll_to_focused_item` flag decouples "request scroll" from "perform scroll", avoiding conflicts with the render loop's normal scroll management.

- `scroll_position_to_show_item()` uses cached item heights for accuracy but degrades gracefully to estimates when cache is cold, self-correcting on the next render.

- Explicit `follow` management ensures the viewport behaves predictably: live-tail when not focused, hold-position when navigating, return-to-live on explicit exit.

## Prevention Strategies

**Code Review Checklist:**
- [ ] Do keyboard navigation handlers use `find_prev_navigable`/`find_next_navigable`?
- [ ] Is `scroll_to_focused_item` set when focus changes?
- [ ] Is `follow` explicitly managed on focus entry and exit paths?

**Testing:**
- Manual test: Navigate through transcript with Up/Down, verify focus skips dividers.
- Manual test: Focus an item, scroll away, press Up/Down, verify viewport snaps to show focused item.
- Manual test: Focus an item, press Esc, verify viewport returns to live tail.

**Patterns to Follow:**
- For TUI scroll state management, prefer one-shot flags over continuous state updates.
- Centralize navigation eligibility checks on the data type rather than scattering logic across handlers.
