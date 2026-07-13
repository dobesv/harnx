---
title: "AG-UI transcript layout, text/tool interleaving, and reload tool detail"
date: 2026-07-12
category: "integration-issues"
problem_type: integration_issue
component: "harnx-serve"
root_cause: "CSS overlay hack, AG-UI text message ID reuse, and frontend reliance on non-snapshot custom events"
resolution_type: code_fix
severity: high
tags:
  - ag-ui
  - assistant-ui
  - css
  - flexbox
  - streaming
  - tool-calls
  - snapshot
  - reload
plan_ref: "web-ui-transcript-tools"
---

## Problem

Three harnx web UI bugs:
1. Message composer overlaid transcript bottom, hiding end of scroll region
2. Live streaming text→tool→text showed post-tool text merged into pre-tool text block (wrong order)
3. Page reload lost tool-call summary/args detail that live view showed

## Symptoms

```text
# Bug 1 — CSS overlay
Bottom of transcript hidden under composer.
Scrollbar bottom not reachable.
Magic 180px padding hack compensated poorly.

# Bug 2 — Text/tool interleave order
Assistant streams "Hello" → tool call → "World".
UI shows: "HelloWorld" (merged), then tool card at bottom.
Reload fixes order but snapshot path loses tool detail (bug 3).
TUI showed correct order throughout.

# Bug 3 — Reload detail loss
Live tool card: "bash_exec $ acli jira workitem view ..."
Reloaded tool card: "bash_exec" (name only, no args/summary).
```

## Investigation Steps

### CSS Layout

Traced `chat.css`: `.aui-thread-bottom` used `position:absolute; bottom:0` overlaying `.aui-thread-viewport`. Viewport compensated with `padding-bottom:180px`. Gradient overlay + `pointer-events:none` hid the overlap. Scroll region bottom always hidden.

### Text/Tool Interleave

Inspected `crates/harnx-serve/src/ag_ui.rs` and `session_actor.rs`:
- `session_actor.rs` emitted ONE `TEXT_MESSAGE_START` upfront with single `message_id` for whole run
- `AgUiSink.emit_text_delta` reused same `message_id` for all text chunks
- `emit_tool_event(Started)` never closed the text message before emitting tool events
- `@assistant-ui/react-ag-ui` run-aggregator keys text parts by `message_id` — same ID = same part

Verified: post-tool content appended to pre-tool text part because ID matched.

### Reload Tool Detail

Live cards showed command/args via custom `tool_summary` SSE event stored in frontend `toolSummaries` map. Reload: `MESSAGES_SNAPSHOT` carries standard `tool_calls[].function.arguments` but NOT custom events. `ToolCallCard` collapsed header only rendered `toolName + summaryMarkdown`. Args existed but weren't surfaced in collapsed header.

## Root Cause

### 1. CSS

Absolute-positioned composer over scrollable viewport required magic padding workaround. Layout not robust to composer height changes.

### 2. Text/Tool Interleave

AG-UI protocol and assistant-ui treat ONE text message ID as ONE contiguous text part. Backend reused same ID across tool boundaries. No `TEXT_MESSAGE_END` emitted before tool start, no fresh `TEXT_MESSAGE_START` after tool end.

### 3. Reload Detail

Frontend depended on live-only custom SSE event for collapsed preview. Snapshot carried args but component didn't derive preview from them.

## Solution

### 1. CSS — Flexbox Column

```css
/* Before: overlay hack */
.aui-thread-bottom {
  position: absolute;
  bottom: 0;
}
.aui-thread-viewport {
  padding-bottom: 180px; /* magic */
}

/* After: proper flex */
.aui-thread {
  display: flex;
  flex-direction: column;
  height: 100%;
}
.aui-thread-viewport {
  flex: 1 1 auto;
  min-height: 0; /* crucial for flex child to shrink and scroll */
  overflow-y: auto;
}
.aui-thread-bottom {
  flex: 0 0 auto;
}
```

Removed: absolute positioning, gradient overlay, pointer-events hack, 180px padding.

Key insight: `min-height: 0` on flex child with `overflow-y: auto` enables scroll within flex container. Without it, child expands parent instead of scrolling.

### 2. Backend — Text Segmentation Around Tool Calls

`AgUiSink` now owns `Mutex<TextSegmentState>` with `Option<MessageId>`:

```rust
// ag_ui.rs - lazy text segment open
fn ensure_text_message_started(&self) -> MessageId {
    let mut state = self.text_segment_state.lock().expect("...");
    if let Some(id) = state.open_message_id.as_ref() {
        return id.clone();
    }
    let new_id = MessageId::new_random();
    self.emit_event(Event::TextMessageStart(TextMessageStartEvent {
        message_id: new_id.clone(),
    }));
    state.open_message_id = Some(new_id.clone());
    new_id
}

// Close before tool start
fn emit_tool_event(&self, event: ToolEvent) {
    if matches!(event, ToolEvent::Started { .. }) {
        self.close_text_segment(); // TEXT_MESSAGE_END if open
    }
    // ... emit tool events
}
```

`session_actor.rs` changes:
- Removed upfront `TEXT_MESSAGE_START` — sink opens lazily on first text delta
- All THREE terminal paths (success, tool-approval interrupt, error/cancel) call `close_text_segment()` before terminal event
- Sink captured in `RunFinished` via `Arc<BroadcastEventSender>` for error path access

**Critical correctness rule**: every terminal path must close text segment. Missing on error path left UI in permanent "streaming" state.

### 3. Frontend — Args-Derived Fallback Preview

```typescript
// ToolCallCard.tsx - fallback when summaryMarkdown absent
let fallbackPreview: string | null = null;
if (!summaryMarkdown && parsedArgs && typeof parsedArgs === 'object') {
    if (typeof parsedArgs.command === 'string') {
        fallbackPreview = `$ ${parsedArgs.command}`;
    } else if (Object.keys(parsedArgs).length === 1) {
        const val = Object.values(parsedArgs)[0];
        if (typeof val === 'string') fallbackPreview = val;
    }
    if (!fallbackPreview) {
        fallbackPreview = JSON.stringify(parsedArgs).slice(0, 50);
    }
}
```

Primary path unchanged: live summary takes precedence. Fallback only activates when summary missing (reload). Safe: rendered as React text node, no XSS.

MSW mock fix: added assistant message with `tool_calls` array (not just tool result) so tests exercise real snapshot shape.

## Why This Works

### CSS

Flex column with scrollable middle + fixed bottom is the standard pattern. Child with `flex: 1` fills space; `min-height: 0` permits shrinking; `overflow-y: auto` enables scroll. No hacks needed.

### Text Segmentation

AG-UI protocol requires `TEXT_MESSAGE_START`/`END` pairs for each text segment. By closing before tool and reopening with fresh ID after, post-tool text becomes a new part rendered after tool card. `@assistant-ui/react-ag-ui` run-aggregator respects part order from distinct message IDs.

### Args Fallback

Snapshot carries `tool_calls[].function.arguments` reliably. Frontend fallback derives preview from this persisted data, not from transient live event. On reload, collapsed header shows meaningful context.

## Prevention Strategies

### Test Cases

```rust
// Backend — interleave ordering
#[test]
fn ag_ui_sink_segments_text_around_tool_calls() {
    // Text delta → tool start → text delta
    // Assert: TEXT_START(id1), CONTENT(id1), TEXT_END(id1),
    //         TOOL_*, TEXT_START(id2), CONTENT(id2), TEXT_END(id2)
    // Assert: id2 != id1
}

#[test]
fn session_actor_error_closes_text_segment_before_run_error() {
    // Partial text → failure
    // Assert: TEXT_MESSAGE_END before RUN_ERROR
}

#[test]
fn session_actor_interrupt_closes_text_segment_before_run_finished() {
    // Partial text → ToolApprovalInterrupt
    // Assert: TEXT_MESSAGE_END before RUN_FINISHED
}
```

```typescript
// Frontend — fallback rendering
it('shows $ command for bash_exec args', () => {
  const args = { command: 'ls -la' };
  // Assert: collapsed header shows "$ ls -la"
});
```

### Code Review Checklist

- [ ] Is the scrollable flex child given `min-height: 0`?
- [ ] Does every run-exit path (success/interrupt/error) close the text segment?
- [ ] Are text segments closed before tool starts?
- [ ] Does frontend derive reload-safe data from snapshot fields, not custom events?
- [ ] Does MSW mock include assistant `tool_calls` array for realistic snapshot testing?

### Best Practices

1. **Flex scroll pattern**: `flex: 1; min-height: 0; overflow: auto` on scrollable child
2. **AG-UI text segments**: One message ID = one contiguous part. Close/open at tool boundaries.
3. **Terminal path coverage**: Every exit path must close active resources
4. **Snapshot-safe previews**: Derive from persisted fields, not transient live events

## Related Issues

- **Related Solution**: [ag-ui-reload-hydration-stream-follo-2026-07-11.md](./ag-ui-reload-hydration-stream-follo-2026-07-11.md) — Prior tool summary hydration and snapshot fixes
- **Related Solution**: [ag-ui-tool-approval-interrupt-resume-2026-07-08.md](./ag-ui-tool-approval-interrupt-resume-2026-07-08.md) — HITL interrupt/resume mechanics
- **Related Solution**: [ag-ui-server-protocol-integration-2026-07-04.md](./ag-ui-server-protocol-integration-2026-07-04.md) — Initial AG-UI protocol integration

## File Pointers

- `web/src/chat.css`: flexbox thread layout
- `crates/harnx-serve/src/ag_ui.rs`: `TextSegmentState`, `ensure_text_message_started`, `close_text_segment`, `emit_tool_event`
- `crates/harnx-serve/src/session_actor.rs`: `handle_run_done` terminal paths
- `web/src/ToolCallCard.tsx`: `fallbackPreview` logic
- `web/src/mocks/handlers.ts`: MSW mock with assistant `tool_calls`
- `crates/harnx-serve/src/ag_ui_tests.rs`: interleave and terminal-path tests
