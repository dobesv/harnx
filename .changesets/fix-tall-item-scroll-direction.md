---
harnx: patch
---
Fix TUI scroll direction through tall transcript items.

When a transcript entry was taller than the viewport, scrolling up caused the
visible window through the item to slide *forward* through its content (toward
the last line) instead of backward (toward the first line).  At the maximum
scroll-up position the rendered slice was identical to the pinned-to-bottom
view, making the user feel they could not actually scroll past the bottom of a
tall item.

Root cause: the vendored `ratatui_widget_scrolling::PartialBottomItem` carries
the number of lines of the item that are scrolled *above* the viewport, but the
value being passed in was `scroll_offset` — the number of lines scrolled
*below* the viewport.  These only happen to coincide at one specific scroll
position; everywhere else the renderer skipped the wrong portion of the item's
buffer.  The fix passes the correct quantity:
`max(0, item_height - scroll_offset - viewport_height)`.

Also adds a regression test in `harnx-tui` that asserts the exact set of lines
visible at several scroll positions for a 20-line item in a 10-line viewport.
The previous PR #264 test only asserted the presence of *some* line in a
3-line range, which was satisfied by both the buggy and the correct output.
