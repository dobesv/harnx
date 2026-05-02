---
title: "TUI transcript focus navigation with bidirectional exit"
date: 2026-05-01
category: "integration-issues"
problem_type: integration_issue
component: "harnx-tui"
root_cause: "navigation between history and transcript required explicit focus states and clear bidirectional exit paths"
resolution_type: code_fix
severity: medium
tags:
  - tui
  - navigation
  - transcript-focus
  - user-experience
  - focus-state
plan_ref: "harnx-342-phase2"
---

## Problem

TUI needed arrow-key navigation within the transcript for selecting entries to edit/delete. Without explicit focus state and bidirectional exit paths, users could enter transcript navigation but become trapped with no way back to input.

## Symptoms

- Up arrow from empty input entered transcript mode (correct)
- No way to exit transcript mode back to input
- Users stuck navigating transcript with no escape route
- Confusion about whether they were in "history mode" or "transcript mode"

## Investigation Steps

1. Reviewed `handle_up_key` and `handle_down_key` in `input.rs`
2. Found `transcript_focus: Option<usize>` field on App for tracking cursor position
3. Identified the need for explicit entry/exit conditions
4. Designed bidirectional exit: Up at top of transcript returns to history, Down at bottom stays
5. Implemented Shift+Up/Down for range selection extension

## Root Cause

Single state variable (`transcript_focus: Option<usize>`) must communicate two distinct modes:
- `None` → input focused, regular typing/history navigation
- `Some(idx)` → transcript focused, Up/Down navigates within transcript

The transition conditions needed careful design to avoid mode confusion and ensure users can always return to input.

## Solution

Added focus state to App and bidirectional navigation in `crates/harnx-tui/src/types.rs` and `input.rs`:

**State fields (types.rs):**
```rust
pub(super) struct App {
    // ...
    /// Index of the cursor item in the transcript (None = input focused).
    pub(super) transcript_focus: Option<usize>,
    /// Anchor index for shift-select range.
    pub(super) transcript_selection_anchor: Option<usize>,
}
```

**Entry logic (input.rs):**
```rust
fn handle_up_key(&mut self, key: KeyEvent) {
    if self.app.input.line_count() <= 1 && self.app.input.lines()[0].is_empty() {
        // Blank input: enter transcript focus mode
        if self.app.transcript_focus.is_none() {
            // First Up: focus last transcript item
            if !self.app.transcript.is_empty() {
                self.app.transcript_focus = Some(self.app.transcript.len() - 1);
            }
        } else if let Some(0) = self.app.transcript_focus {
            // Up at top: exit transcript focus, return to history
            self.app.transcript_focus = None;
            self.history_prev();
        } else if let Some(focus) = self.app.transcript_focus {
            // Up in middle: move up
            self.app.transcript_focus = Some(focus.saturating_sub(1));
        }
    } else {
        // Text in input: navigate within textarea
        self.app.input.input(ratatui_textarea::Input::from(key));
    }
}

fn handle_down_key(&mut self, key: KeyEvent) {
    if self.app.transcript_focus.is_none() {
        // Input focused: normal history navigation
        self.history_next();
    } else if let Some(focus) = self.app.transcript_focus {
        if focus + 1 < self.app.transcript.len() {
            // Down in middle: move down
            self.app.transcript_focus = Some(focus + 1);
        } else {
            // Down at bottom: exit to input
            self.app.transcript_focus = None;
            self.app.transcript_selection_anchor = None;
        }
    }
}
```

## Why This Works

**Bidirectional exit:**
- Up at top of transcript → history mode (reverse to entry path)
- Down at bottom of transcript → input mode (natural forward exit)

**Mode transitions:**
- `None` → `Some(last)`: First Up on blank input
- `Some(0)` + Up → `None` + history_prev: Exit to history
- `Some(last)` + Down → `None`: Exit to input

**Shift+Up/Down for selection:**
```rust
fn handle_up_key_shift(&mut self) {
    if let Some(focus) = self.app.transcript_focus {
        if self.app.transcript_selection_anchor.is_none() {
            self.app.transcript_selection_anchor = Some(focus);
        }
        self.app.transcript_focus = Some(focus.saturating_sub(1));
    }
}
```

This enables range selection for multi-entry operations (delete range, etc.).

## Prevention Strategies

**Test cases:**
- Up on blank input enters transcript mode
- Up at top of transcript exits to history
- Down at bottom of transcript exits to input
- Shift+Up/Down extends selection range

**Best practices:**
- Focus states should always have clear entry AND exit paths
- Navigation should feel reversible — users can always back out
- Avoid "mode traps" where users can't return to previous mode

**Code review checklist:**
- [ ] Does every mode entry have a corresponding exit?
- [ ] Can users escape from any state?
- [ ] Are navigation paths documented?

## Related Issues

- **PR:** #423 — Non-destructive session history editing
- **Requirement:** D1-D3 — Transcript selection state and navigation
