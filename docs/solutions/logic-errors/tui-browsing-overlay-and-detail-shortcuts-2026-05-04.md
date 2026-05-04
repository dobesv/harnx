---
title: "TUI browsing mode overlay and detail view mutation shortcuts"
date: 2026-05-04
category: "logic-errors"
problem_type: logic_error
component: "harnx-tui"
root_cause: "multiple overlapping UX issues in transcript navigation: scroll state defaults, modal render order, focus loss during edit, and missing input guards"
resolution_type: code_fix
severity: high
tags:
  - tui
  - overlay
  - scroll-state
  - modal
  - ratatui
  - navigation
  - focus
plan_ref: "harnx-tui-transcript-navigation-ux"
---

## Problem

Four related UX bugs in harnx-tui transcript navigation: (1) detail view opened scrolled to bottom instead of top, (2) focused transcript items could scroll off-screen, (3) mutation shortcuts (e/d/r/c) were unavailable in detail view, and (4) confirmation modals rendered behind the detail view overlay.

## Symptoms

- **#441:** Detail view opened at bottom of content; user had to scroll up to see beginning
- **#433/#454:** Focused transcript item could scroll out of visible area during browsing, losing context
- **#455:** Pressing `e`, `d`, `r`, `c` in detail view did nothing — user had to exit to run mutations
- **Modal render bug:** Delete/rewind confirmation modal appeared behind the detail view overlay, invisible to user

## Investigation Steps

1. Traced `detail_view_scroll` initialization — found default `ScrollState::new()` sets `follow = true`, causing immediate scroll-to-bottom on first render
2. Identified `transcript_browsing` mode as solution for #433/#454 — allows fullscreen history navigation while keeping focused item visible
3. Discovered modal rendered after `render_detail_view()` return, preventing modal from appearing on top
4. Found `handle_transcript_edit()` clears `transcript_focus` and `transcript_browsing`, breaking detail view reopen after edit

## Root Cause

**Scroll state default:** `ScrollState::new()` defaults `follow = true`, auto-scrolling to bottom. Opening detail view at bottom confused users expecting to see item header first.

**Missing browsing mode:** No fullscreen mode for history navigation; focused items could scroll off-screen as new content arrived.

**Guard ordering:** Mutation shortcuts were only available in the main match block, not inside the `detail_view_open` guard.

**Render order bug:** Modal rendered after fullscreen overlay returned early, so modal appeared behind overlay content.

**Edit-focus loss:** `handle_transcript_edit()` clears focus state. Without saving focus before calling it, the detail view couldn't reopen at the same position.

## Solution

### 1. ScrollState `follow=false` initialization pattern

Always initialize detail view scroll with `follow = false`:

```rust
self.app.detail_view_scroll = {
    let mut s = ratatui_widget_scrolling::ScrollState::new();
    s.follow = false;
    s
};
```

Applied in:
- `handle_key` when opening detail view (lines 361-365)
- `handle_up_key`/`handle_down_key` when entering browsing mode (lines 1256-1260, 1285-1289)
- In detail view edit handler before reopening (lines 173-177)

### 2. Browsing mode overlay pattern

Enter browsing mode on Up/Down from blank input. ESC exits, ENTER opens detail:

```rust
// In handle_key, after modal and detail_view_open guards:
if self.app.transcript_browsing && !self.app.detail_view_open {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, KeyModifiers::NONE) => {
            self.app.transcript_browsing = false;
            self.app.transcript_focus = None;
            self.app.transcript_selection_anchor = None;
            self.app.scroll_state.follow = true;
        }
        (KeyCode::Up, KeyModifiers::NONE) => {
            self.handle_up_key(key);
        }
        (KeyCode::Down, KeyModifiers::NONE) => {
            self.handle_down_key(key);
        }
        (KeyCode::Enter, KeyModifiers::NONE) => {
            // Open detail view for current focused item
            self.app.detail_view_scroll = {
                let mut s = ratatui_widget_scrolling::ScrollState::new();
                s.follow = false;
                s
            };
            self.app.detail_view_raw_yaml = self
                .selected_seq_range()
                .and_then(|(from, to)| self.config.read().get_message_range_yaml(from, to));
            self.app.detail_view_open = true;
        }
        (KeyCode::Char('e'), KeyModifiers::NONE) => {
            self.handle_transcript_edit().await?;
        }
        (KeyCode::Char('i'), KeyModifiers::NONE) => {
            self.handle_transcript_insert();
        }
        // ... other mutation shortcuts ...
        _ => {} // consume all other keys to prevent bleed to input
    }
    return Ok(());
}
```

### 3. Modal-on-top-of-overlay render fix

Render modal after overlay content, before return:

```rust
// In draw():
if self.app.detail_view_open {
    self.render_detail_view(frame, size);
    // Render modal ON TOP of detail view if active
    if let Some(modal) = &self.app.modal.clone() {
        self.render_modal(frame, size, modal);
    }
    return;
}

if self.app.transcript_browsing {
    self.render_browsing_view(frame, size);
    if let Some(modal) = &self.app.modal.clone() {
        self.render_modal(frame, size, modal);
    }
    return;
}
```

Key insight: modal must render **after** overlay content but **before** the early return.

### 4. Edit-flow focus preservation pattern

Save focus before calling handler that clears it:

```rust
(KeyCode::Char('e'), KeyModifiers::NONE) => {
    // Save focus before editing since handle_transcript_edit clears it
    let had_focus = self.app.transcript_focus;
    self.app.detail_view_open = false;
    self.handle_transcript_edit().await?;
    // After edit, try to reopen detail view at same position
    if let Some(focus_idx) = had_focus {
        if focus_idx < self.app.transcript.len() {
            self.app.transcript_focus = Some(focus_idx);
            self.app.transcript_selection_anchor = None;
            self.app.detail_view_scroll = {
                let mut s = ratatui_widget_scrolling::ScrollState::new();
                s.follow = false;
                s
            };
            self.app.detail_view_raw_yaml =
                self.selected_seq_range().and_then(|(from, to)| {
                    self.config.read().get_message_range_yaml(from, to)
                });
            self.app.detail_view_open = true;
        }
    }
}
```

### 5. Guard ordering in handle_key

Order: modal → detail_view → browsing_mode → main match:

```rust
pub(super) async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
    // 1. Modal guard — highest priority
    if self.app.modal.is_some() {
        return self.handle_modal_key(key).await;
    }

    // 2. Detail view guard — navigation + mutations inside
    if self.app.detail_view_open {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, KeyModifiers::NONE) => { /* close */ }
            (KeyCode::Up, KeyModifiers::NONE) => { /* scroll up */ }
            (KeyCode::Down, KeyModifiers::NONE) => { /* scroll down */ }
            (KeyCode::Char('e'), KeyModifiers::NONE) => { /* edit with focus save */ }
            (KeyCode::Char('d'), KeyModifiers::NONE) => { /* delete */ }
            (KeyCode::Char('r'), KeyModifiers::NONE) => { /* rewind */ }
            (KeyCode::Char('c'), KeyModifiers::NONE) => { /* copy */ }
            _ => {} // consume all other keys
        }
        return Ok(());
    }

    // 3. Browsing mode guard
    if self.app.transcript_browsing && !self.app.detail_view_open {
        match (key.code, key.modifiers) { /* ... */ }
        return Ok(());
    }

    // 4. Main match block — normal input handling
    match (key.code, key.modifiers) { /* ... */ }
}
```

## Why This Works

**`follow=false` initialization:** Prevents automatic scroll-to-bottom. User sees item header first, matching mental model of "opening a document."

**Browsing mode overlay:** Fullscreen history navigation keeps focused item visible in dedicated `browsing_view_scroll` state, isolated from live transcript scroll. User can explore history without losing position.

**Modal render order:** Ratatui renders widgets in call order. Rendering modal after overlay content but before return ensures modal appears on top visually.

**Focus preservation:** Saving `transcript_focus` before calling edit handler allows reopening detail view at same position after transcript reload. Without this, focus would be `None` and detail view wouldn't reopen.

**Guard ordering:** Top-down guard structure ensures each mode intercepts relevant keys before they reach lower-priority handlers. Catch-all `_ => {}` arms prevent key bleed-through.

## Prevention Strategies

**Test cases:**
- Test detail view opens at top (position 0), not bottom
- Test browsing mode enters on Up from blank input, exits on ESC
- Test mutation shortcuts (e/d/r/c) work in detail view
- Test modal renders on top of detail/browsing overlay
- Test edit from detail view reopens detail at same position

**Best practices:**
- Always initialize `ScrollState` with `follow = false` for static content views
- Render modals after overlay content, before early return
- Save focus state before calling handlers that clear it
- Order input guards from highest to lowest priority: modal → overlay → main
- Use catch-all `_ => {}` to consume unhandled keys in exclusive modes

**Code review checklist:**
- [ ] Is `ScrollState` initialized with `follow = false` for non-live views?
- [ ] Does modal render after overlay content in draw()?
- [ ] Is focus saved before calling handlers that clear it?
- [ ] Is guard ordering correct: modal → detail_view → browsing → main?
- [ ] Do overlay guards have catch-all consumption arms?

## Related Issues

- **GitHub:** [#441](https://github.com/dobesv/harnx/issues/441) — Detail view opens at bottom
- **GitHub:** [#433](https://github.com/dobesv/harnx/issues/433) — Keep focused item visible
- **GitHub:** [#454](https://github.com/dobesv/harnx/issues/454) — Fullscreen history browsing
- **GitHub:** [#455](https://github.com/dobesv/harnx/issues/455) — Mutation shortcuts in detail view
- **Related Solution:** [logic-errors/tui-exclusive-overlay-pattern-2026-05-02.md](./tui-exclusive-overlay-pattern-2026-05-02.md) — Base overlay pattern for detail view
