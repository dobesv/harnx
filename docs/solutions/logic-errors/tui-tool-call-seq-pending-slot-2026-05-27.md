---
title: "TUI tool call seq assignment via pending slot for inverted event ordering"
date: 2026-05-27
category: "logic-errors"
problem_type: logic_error
component: "harnx-tui"
root_cause: "event ordering mismatch — log seq assigned before ToolCall transcript item created"
resolution_type: code_fix
severity: medium
tags:
  - tui
  - event-ordering
  - transcript
  - tool-calls
  - state-machine
  - pending-slot
plan_ref: "issue-626-detail-view-tool-output"
---

## Problem

In the harnx TUI, pressing Enter on a ToolCall item in transcript browsing mode opened a detail view showing only the rendered markdown/yaml body — same as the main view — instead of the full raw session YAML with both tool call inputs AND outputs.

## Symptoms

- Detail view for live (non-history) tool calls showed `body (markdown): [command]` without result
- `detail_view_raw_yaml` was `None` for live tool calls
- History items loaded from session file worked correctly
- Issue reproducible: any tool executed during live session, then viewed in browsing mode

## Investigation Steps

1. Traced `detail_view_raw_yaml` assignment in `input.rs:237-248` — depends on `selected_seq_range()` returning valid seq
2. Checked `selected_seq_range()` — returns `None` when transcript item has `seq == None`
3. Traced ToolCall creation in `input.rs:1134-1140` — `ToolEvent::Started` creates `TranscriptItem::ToolCall { seq: None }`
4. Found `LogSeqAssigned` handler in `input.rs:895-916` — backfills seq onto most recent unsequenced item
5. Discovered event ordering: `LogSeqAssigned` fires BEFORE `ToolEvent::Started` creates the ToolCall
6. Backfill found no ToolCall to patch → seq lost
7. History items work because `messages_to_transcript_items` reconstructs items with seq already set

## Root Cause

TUI event ordering for live tool calls is inverted:

1. `LogSeqAssigned { seq }` fires before `ToolEvent::Started` creates the ToolCall transcript item
2. Backfill handler tries to find most recent unsequenced item — no ToolCall exists yet
3. `ToolEvent::Started` creates `TranscriptItem::ToolCall { seq: None }` — never backfilled
4. `selected_seq_range()` returns `None` for seq-less items
5. `get_message_range_yaml()` cannot fetch session content
6. `detail_view_raw_yaml` stays `None` → fallback renderer used (no tool result shown)

This contrasts with text items (UserText, AssistantText) where the transcript item is created before seq assignment arrives.

**Key insight:** ToolCalls are unique in that persistence happens BEFORE the tool runs, so the seq is known before the UI event fires.

## Solution

Added `pending_tool_seq: Option<usize>` to the `App` struct to bridge the event ordering gap.

**Lifecycle:**

1. `LogSeqAssigned` arrives and no existing item to backfill → `pending_tool_seq = Some(seq)`
2. `LogSeqAssigned` arrives and finds item to backfill → `pending_tool_seq = None` (seq consumed)
3. `ToolEvent::Started` or `ToolEvent::Blocked` creates ToolCall → uses `pending_tool_seq` as seq
4. `pending_tool_seq` NOT cleared after ToolCall creation — multiple tool calls in same round share same log_seq
5. Overwritten by next `LogSeqAssigned` from next round

**Code changes:**

```rust
// types.rs
pub(super) pending_tool_seq: Option<usize>,

// input.rs — LogSeqAssigned handler
if backfilled {
    self.app.pending_tool_seq = None;
} else {
    self.app.pending_tool_seq = Some(seq);
}

// input.rs — ToolEvent::Started handler
AgentEvent::Tool(ToolEvent::Started { .. }) => {
    vec![TranscriptItem::ToolCall {
        seq: self.app.pending_tool_seq,  // Use pending slot
        ..
    }]
}
```

Also improved fallback renderer in `render.rs` to peek at adjacent `ToolResultMarkdown` when selected item is a seq-less ToolCall at end of selection range.

## Why This Works

The pending slot decouples the seq assignment event from the ToolCall creation event. Since LogSeqAssigned arrives before ToolEvent::Started, we store the seq and retrieve it when the ToolCall is created. Multiple tools in the same round share the seq (one `ToolCalls` log entry covers all), so we intentionally don't clear it after first use.

This pattern mirrors existing `pending_thought_text` and `pending_message` slots but handles the inverse ordering — those are for items created BEFORE seq assignment, ToolCalls are created AFTER.

## Prevention Strategies

**Test cases:**
- `live_log_seq_assignment_applies_to_tool_calls_started_after_seq_event` — primary case: LogSeqAssigned then ToolEvent::Started
- `log_seq_backfill_clears_pending_tool_seq` — backfill path clears pending
- `live_log_seq_assignment_applies_to_blocked_tool_calls` — Blocked path uses same pattern

**Best practices:**
- Any TUI state set by one agent event and consumed by later event needs an intermediary "pending" slot
- When event ordering differs between live and history replay, test both paths
- Trace the full lifecycle: `LogSeqAssigned` → `TranscriptItem` creation → `selected_seq_range()` → `detail_view_raw_yaml`

**Code review checklist:**
- [ ] Does the item exist in transcript before seq assignment?
- [ ] If not, does a pending slot capture the seq?
- [ ] Is the pending slot cleared appropriately to avoid stale state?

## Related Issues

- **GitHub:** [#626](https://github.com/dobesv/harnx/issues/626) — Detail view not showing full tool output
- **Related Solution:** [tui-browsing-full-transcript-render-2026-05-05.md](./tui-browsing-full-transcript-render-2026-05-05.md) — Browsing mode transcript view architecture
- **Related Solution:** [tool-output-header-suppression-2026-05-02.md](./tool-output-header-suppression-2026-05-02.md) — Tool output display patterns
