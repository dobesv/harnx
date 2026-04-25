---
harnx: patch
---
Fix TUI scroll bugs with tall transcript items (#264).

Two bugs caused the scroll position to appear "stuck" when a transcript entry
(e.g. a long tool call) was taller than the viewport:

1. **Wrong content slice for tall items** — `copy_partial_bottom_widget_to_frame`
   always showed the *first* `viewport_height` lines of a tall item regardless of
   how many lines were hidden above the viewport.  The fix (vendored patch to
   `ratatui_widget_scrolling`) skips the hidden top lines so the correct window of
   content is displayed, matching the behaviour of `copy_partial_top_widget_to_frame`.

2. **Stale-max dead zone on scroll-up** — `scroll_down()` clamps against
   `last_max_position` from the *previous* render frame.  When the height cache
   catches up at render time `last_max_position` jumps up, but `position` was
   already clamped to the stale (too-small) ceiling, so every subsequent
   `scroll_up` tick burned off the ghost excess before any visual movement
   occurred.  The fix clamps `position` to the freshly-updated
   `last_max_position` immediately after each render, eliminating the dead zone.
