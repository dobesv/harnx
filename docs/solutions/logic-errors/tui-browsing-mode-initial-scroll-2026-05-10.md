---
title: "TUI browsing mode initial scroll to last item (dead code flag consumption and empty height cache)"
date: 2026-05-10
category: logic-errors
problem_type: logic_error
component: harnx-tui
root_cause: "flag consumed by dead code in wrong render path, and empty height cache on first browsing entry"
resolution_type: code_fix
severity: medium
tags:
  - tui
  - scroll-state
  - browsing-mode
  - ratatui
  - height-cache
  - flag-lifecycle
plan_ref: "harnx-507-history-nav-scroll"
---

## Problem

Pressing UP to enter browsing mode in the TUI would select the last transcript item but the viewport remained at the top of the transcript instead of scrolling the focused item into view. Footer showed "Item 850 of 850" while the screen displayed items from the top.

## Symptoms

- Pressing UP from blank input line focused last navigable item but viewport stayed at top
- `transcript_focus` set correctly (e.g., index 850)
- `scroll_to_focused_item` flag set to `true` by input handler
- After render, `browsing_view_scroll.position` remained 0
- Only reproducible on first entry into browsing mode with transcript taller than viewport

## Investigation Steps

1. Traced `handle_key(Up)` in `input.rs` — confirmed `transcript_focus = Some(last_navigable)` and `scroll_to_focused_item = true` set correctly.
2. Added debug logging to `render_browsing_view` — discovered flag was always `false` when browsing view rendered.
3. Searched for all consumers of `scroll_to_focused_item` flag in `render.rs`.
4. Found **dead code block** in non-browsing `draw()` path (lines ~420-450) that consumed and cleared the flag even when `transcript_browsing = true`.
5. Confirmed: every setter of `scroll_to_focused_item` also sets `transcript_browsing = true` — the flag is only meaningful in browsing-mode code paths.
6. Even after fixing flag consumption, position calculation was still wrong — traced to `scroll_position_to_show_item()` using empty height cache.
7. `browsing_view_scroll` is a separate `ScrollState` that starts with empty `render_height_cache`, so all items default to height=1, producing wildly incorrect scroll positions for multi-line items.

## Root Cause

**Bug 1 — Dead code consuming the flag:**

The non-browsing render path in `draw()` contained a block that checked and cleared `scroll_to_focused_item`:

```rust
// Dead code in non-browsing path
if self.app.scroll_to_focused_item {
    // ... scroll logic for normal view
    self.app.scroll_to_focused_item = false;
}
```

This block ran even when `transcript_browsing = true`, consuming the flag before `render_browsing_view` could see it. Since `scroll_to_focused_item` is only ever set when entering or navigating within browsing mode (every setter also sets `transcript_browsing = true`), this code should never execute when browsing is active.

**Bug 2 — Empty height cache:**

`browsing_view_scroll` is a separate `ScrollState` initialized with an empty `render_height_cache`. On first entry into browsing mode, `scroll_position_to_show_item()` has no cached heights and defaults all items to height=1. For multi-line transcript items (messages, code blocks), this produces incorrect scroll positions — often an order of magnitude wrong.

## Solution

### 1. Remove dead code from non-browsing path

Removed the entire `scroll_to_focused_item` block from the non-browsing `draw()` path. The non-browsing view has no concept of a focused item — this code was never intended to exist there.

**Key insight:** The `scroll_to_focused_item` flag is **only ever set when entering or navigating within browsing mode** — every setter in `input.rs` also sets `transcript_browsing = true`. The non-browsing render path never has a reason to process this flag.

### 2. Prime height cache from main scroll state

Added `copy_height_cache_from()` method to `ScrollState` in `ratatui-widget-scrolling`:

```rust
/// Copy the height cache from `other` into `self`.
///
/// This is used when a secondary scroll view (e.g. the browsing-mode overlay)
/// needs to immediately compute an accurate scroll position for
/// `scroll_position_to_show_item` without having rendered any content yet.
pub fn copy_height_cache_from(&mut self, other: &Self) {
    self.render_height_cache = other.render_height_cache.clone();
}
```

Inside `render_browsing_view`, before computing the scroll position:

```rust
if self.app.scroll_to_focused_item {
    if let Some(focus) = self.app.transcript_focus {
        // Prime the browsing-view height cache from the main scroll state so that
        // scroll_position_to_show_item has accurate per-item heights even on the
        // very first render of the browsing overlay.
        self.app
            .browsing_view_scroll
            .copy_height_cache_from(&self.app.scroll_state);
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

The main `scroll_state` has already rendered the same items at the same width, so its height cache is accurate. Copying it primes `browsing_view_scroll` with correct heights before the first position calculation.

### 3. Regression test

Added `test_browsing_mode_first_up_scrolls_last_item_into_view` in `tests.rs`:

```rust
#[tokio::test]
async fn test_browsing_mode_first_up_scrolls_last_item_into_view() {
    let mut harness = TuiTestHarness::with_size(60, 12);

    // 20 items in a 10-row viewport
    for i in 0..20usize {
        harness.tui().app.transcript.push(TranscriptItem::UserText {
            text: format!("Message {i}"),
            seq: None,
            timestamp: None,
        });
    }

    harness.render(); // Prime main scroll_state height cache

    // Press UP to enter browsing mode
    harness.tui().handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)).await.unwrap();

    assert!(harness.tui().app.transcript_browsing);
    assert_eq!(harness.tui().app.transcript_focus, Some(19));

    harness.render();

    // Position > 0 means we scrolled toward the focused item
    let pos = harness.tui().app.browsing_view_scroll.position;
    assert!(pos > 0, "position should be > 0, got {pos}");

    // The last message must be visible on screen
    let screen = harness.screen_contents();
    assert!(screen.contains("Message 19"));
}
```

## Why This Works

**Flag lifecycle correction:** By removing the dead code block, `scroll_to_focused_item` now survives from the input handler through to `render_browsing_view`. The browsing view renderer is the sole consumer of this flag, matching its sole producer (browsing-mode input handlers).

**Height cache priming:** The main `scroll_state` has accurate per-item heights from its normal rendering loop. Copying this cache to `browsing_view_scroll` before computing position ensures `scroll_position_to_show_item()` sees accurate heights even before the browsing view has rendered anything itself. The first position calculation is correct rather than an order-of-magnitude estimate.

## Prevention Strategies

**Code review checklist:**

- [ ] When a flag/field is only meaningful in one mode (browsing vs normal), ensure only that mode's code paths consume it
- [ ] When adding new `ScrollState` instances, consider whether they need height cache priming from existing state
- [ ] Check all flag consumers when adding new flag producers — ensure producer/consumer are in the same mode

**Testing patterns:**

- Always test first-entry scenarios for stateful overlays (height cache starts empty)
- Verify flag lifecycle: set flag, render once, assert flag consumed, assert state changed
- When copying caches between states, test that cache is primed before dependent calculation runs

**Related patterns:**

- The `scroll_to_focused_item` flag is a "one-shot request" pattern — set by input handler, consumed by renderer on next frame, then cleared. This pattern requires exactly one consumer.
- Secondary `ScrollState` instances may need cache priming from primary state when computing positions before their first render.

## Related Issues

- **GitHub:** [#507](https://github.com/dobesv/harnx/issues/507) — History nav scroll to selected item
- **Related Solution:** [logic-errors/tui-browsing-full-transcript-render-2026-05-05.md](./tui-browsing-full-transcript-render-2026-05-05.md) — Full transcript rendering and scroll state ownership in browsing mode
- **Related Solution:** [logic-errors/tui-transcript-navigation-focus.md](./tui-transcript-navigation-focus.md) — Keyboard navigation with focus management and auto-scroll (introduced `scroll_to_focused_item` flag)
