---
title: "TUI compaction marker as single navigable event with detail view"
date: 2026-06-12
category: integration-issues
problem_type: integration_issue
component: "harnx-tui"
root_cause: "aggregated transcript items were rendered as non-navigable SystemText, preventing users from viewing compacted content as one event"
resolution_type: code_fix
severity: medium
tags:
  - tui
  - ratatui
  - transcript
  - compaction
  - navigation
  - detail-view
plan_ref: view-compaction-details
---

## Problem

After session compaction, the TUI transcript showed a `─── session compacted ───` marker as a plain non-navigable `TranscriptItem::SystemText`. Compacted messages expanded into individual transcript items. Pressing UP highlighted individual lines and ENTER showed just one message — users couldn't view the full compacted output as one navigable event.

## Symptoms

- Compaction marker appeared as dim text, not selectable via UP/DOWN
- Compacted messages rendered as separate transcript lines
- ENTER on any compacted line showed only that single message
- No way to view the complete compacted session as one coherent event

## Investigation Steps

1. Examined `TranscriptItem` enum in `types.rs` — only `UserText`, `AssistantText`, `ToolCall` had `seq()` returning `Some`, making them navigable
2. Traced `session_history_transcript_items()` in `lifecycle.rs` — expanded `compressed_messages` into individual `TranscriptItem`s with a trailing `SystemText` marker
3. Reviewed `is_navigable()` — `SystemText` returns `false`, so marker wasn't focusable
4. Identified that `messages_to_transcript_items()` skips `MessageRole::System`, omitting system messages from detail text
5. Found live `CompactingCompleted` event handler appended markers but didn't rebuild transcript, leaving stale items visible

## Root Cause

Three design gaps:

1. **String-matching for marker type**: Using `SystemText` prevented type-specific behavior and required fragile string matching to detect compaction markers
2. **No navigable aggregate pattern**: Transcript items without message seq couldn't be focused, even when they represent viewable aggregate events
3. **Live/reload path divergence**: History rebuild (clean slate) vs. live event append (accumulates) produced different transcript states

## Solution

### 1. Dedicated transcript item variant

Added `TranscriptItem::CompactionMarker { text, from_seq, to_seq, detail_text }` in `types.rs`:

```rust
pub enum TranscriptItem {
    // ... existing variants ...
    CompactionMarker {
        text: String,           // "─── session compacted ───"
        from_seq: Option<usize>,
        to_seq: Option<usize>,
        detail_text: String,    // Pre-rendered human-readable content
    },
}
```

**Key design:** `is_navigable()` returns `true` for `CompactionMarker`, but `seq()` returns `None`. This makes the marker focusable for viewing while keeping edit/delete/rewind operations message-only.

### 2. Navigation model

In `types.rs`:

```rust
impl TranscriptItem {
    pub fn is_navigable(&self) -> bool {
        matches!(self, Self::UserText { .. } | Self::AssistantText { .. } 
                     | Self::ToolCall { .. } | Self::CompactionMarker { .. })
    }
    
    pub fn seq(&self) -> Option<usize> {
        match self {
            Self::UserText { seq, .. } => Some(*seq),
            Self::AssistantText { seq, .. } => Some(*seq),
            Self::ToolCall { seq, .. } => Some(*seq),
            _ => None,  // CompactionMarker intentionally excluded
        }
    }
}
```

Adding navigable items without seq is the right pattern for view-only aggregate events.

### 3. Detail view dual-state pattern

Added `detail_view_text: Option<String>` to `App` struct alongside existing `detail_view_raw_yaml`:

```rust
pub struct App {
    // ... existing fields ...
    pub detail_view_raw_yaml: Option<String>,  // YAML for seq-based messages
    pub detail_view_text: Option<String>,      // Plain text for CompactionMarker
}
```

Render priority in `render.rs`:

```rust
fn render_detail_view(...) {
    if let Some(text) = &app.detail_view_text {
        // Render plain text with title "Compacted session"
    } else if let Some(yaml) = &app.detail_view_raw_yaml {
        // Render YAML documents
    } else {
        // Fallback: render_entry_detail()
    }
}
```

### 4. Detail text construction

In `lifecycle.rs`, `build_compaction_marker()` prefetches `detail_text` by flattening compressed messages:

```rust
fn build_compaction_marker(messages: &[Message], config: &GlobalConfig) -> TranscriptItem {
    let detail_text = messages_to_transcript_items(messages, config)
        .into_iter()
        .flat_map(|item| flatten_transcript_item_to_compaction_lines(item))
        .collect::<Vec<_>>()
        .join("\n");
    
    TranscriptItem::CompactionMarker {
        text: "─── session compacted ───".into(),
        from_seq: min_seq,
        to_seq: max_seq,
        detail_text,
    }
}
```

**Critical:** Include `MessageRole::System` in `messages_to_transcript_items` and add `compaction_section_label` arm for system messages (`── system ──`). System messages contribute to token usage and belong in the compacted view.

### 5. Live event handler MUST rebuild

**CRITICAL gotcha:** The live `CompactingCompleted` event handler must REBUILD the transcript from `session_history_transcript_items(&config)`, NOT append a marker:

```rust
SessionEvent::CompactingCompleted => {
    // WRONG: self.app.transcript.push(marker);
    // RIGHT:
    self.app.transcript = Self::session_history_transcript_items(&self.config);
    
    // Reset transcript-index-dependent state:
    self.app.streaming_assistant_idx = None;
    self.reset_usage_tracking();
    self.app.transcript_focus = None;  // Or clamp to valid range
    self.app.transcript_selection_anchor = None;
    self.app.last_ui_output_source = None;  // Else next same-source output skips SourceHeading
}
```

**Why:** Without rebuild, just-compacted messages remain visible (duplication), and cumulative `compressed_messages` produce overlapping markers on repeated compactions.

### 6. Centralized detail-open helper

In `input.rs`, unify ENTER handling across modes:

```rust
fn open_detail_view_for_focused_item(&mut self) {
    let focused = self.app.transcript.get(focus_idx);
    match focused {
        Some(TranscriptItem::CompactionMarker { detail_text, .. }) => {
            // The detail view always renders detail_text for a marker, so a
            // seq-based YAML lookup here would be computed but never shown.
            // Use the precomputed detail_text exclusively and leave raw_yaml None.
            self.app.detail_view_text = Some(detail_text.clone());
            self.app.detail_view_raw_yaml = None;
        }
        Some(other) if other.seq().is_some() => {
            self.app.detail_view_text = None;  // Clear for normal messages
            // ... populate detail_view_raw_yaml from seq range ...
        }
        _ => {}
    }
    self.app.detail_view_open = true;
}
```

## Why This Works

1. **Dedicated variant**: Localizes behavior, carries data needed (detail_text + optional seq range), avoids string-matching
2. **seq() returns None**: Edit/delete/rewind (via `selected_seq_range()`) operate only on messages, not aggregate markers
3. **`is_navigable()` controls arrow-key focus**: Marker participates in UP/DOWN navigation and ENTER detail view
4. **Rebuild on live event**: Ensures live and reload paths converge, removing stale items and preventing cumulative marker bugs
5. **Prefetched detail_text**: Avoids computing seq-based YAML for items that always render plain text
6. **Dual-state render priority**: `detail_view_text` first (for markers), `detail_view_raw_yaml` second (for messages)

## Prevention Strategies

**Test Cases:**
- History reload: single navigable marker, no expanded compressed items
- Navigation: UP/DOWN focuses marker
- ENTER: opens detail view showing full `detail_text`
- Live `CompactingCompleted`: assert transcript equals `session_history_transcript_items()` output
- Multiple compactions: one marker after N compactions, no duplicates
- System messages: included in compacted detail text

**Code Review Checklist:**
- [ ] Does new `TranscriptItem` variant need `is_navigable()`?
- [ ] Should edit/delete/rewind operate on this item? If not, `seq()` returns `None`
- [ ] Do live event and reload paths converge? (Both call same builder)
- [ ] Is transcript-index-dependent state reset on rebuild?
- [ ] Is `detail_view_text` cleared when opening normal message detail?

## Related Issues

- **Plan:** view-compaction-details (GH-562)
- **Related Solution:** [tui-compaction-spinner-corruption-2026-06-08.md](tui-compaction-spinner-corruption-2026-06-08.md) — AgentEvent-based compaction progress
- **Related Solution:** [tui-transcript-focus-navigation-2026-05-01.md](tui-transcript-focus-navigation-2026-05-01.md) — Focus state model
