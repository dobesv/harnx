---
title: "ToolEvent::Blocked for hook-denied tool call visibility"
date: 2026-05-08
category: logic-errors
problem_type: logic_error
component: harnx-engine
root_cause: "blocked tool calls short-circuited before event emission, leaving no trace in transcript"
resolution_type: code_fix
severity: medium
tags:
  - event-emission
  - hooks
  - tool-dispatch
  - tui
plan_ref: issue-356-blocked-tool-tui
---

## Problem

Tool calls blocked by `dcg` or other PreToolUse hooks were invisible in the TUI transcript and CLI output. The engine short-circuited before calling `emit_tool_call_fn`, so no `ToolEvent` reached any consumer.

## Symptoms

- Hook-blocked tool calls left no trace in TUI transcript
- User confirmation denials (Ask hook denied) equally invisible
- Transcript showed no indication why an agent's attempted tool call never executed
- Debugging hook behavior required examining logs outside the UI

## Investigation Steps

1. Traced code path in `harnx-engine/src/tool.rs` `eval_tool_calls`: both `HookResultControl::Block` and `HookResultControl::Ask` deny branch built a `blocked_result` JSON but never called any emit function
2. Verified `emit_tool_call_fn` only invoked after approval (line 148), not before hook evaluation
3. Checked `AgentEvent` enum — no `ToolEvent::Blocked` variant existed
4. Considered alternatives:
   - Repurpose `ToolEvent::Completed` with a `blocked` flag — conflates success/failure
   - Emit `ToolEvent::Started` then `Failed` — misleading, implies tool ran
   - Add dedicated `Blocked` variant — cleanest semantics, clearly distinct from success/failure

## Root Cause

The engine's two-phase dispatch pattern (validation/hooks/confirm → emit Started → dispatch) correctly avoided emitting `Started` for blocked calls (per parallel-tool-dispatch pattern). However, the blocked branches merely built a `blocked_result` JSON and returned early without calling any emit function. This left the TUI and CLI sinks with zero visibility into blocked calls.

The engine had both blocked paths ready to return the rejection, but no mechanism existed to surface this information as an event.

## Solution

### 1. Add `ToolEvent::Blocked` variant

```rust
// crates/harnx-core/src/event.rs
pub enum ToolEvent {
    Started { /* ... */ },
    Progress { /* ... */ },
    Update { /* ... */ },
    Completed { /* ... */ },
    Failed { /* ... */ },
    Blocked {
        id: String,
        name: String,
        input: serde_json::Value,
        reason: String,
    },
}
```

### 2. Add `emit_tool_blocked_fn` to `ToolEvalContext`

```rust
// crates/harnx-engine/src/tool.rs
pub struct ToolEvalContext {
    // ...existing fields...
    pub emit_tool_blocked_fn: Arc<ToolCallEmitFn>,
}
```

Call it in both blocked branches:

```rust
// HookResultControl::Block path
let blocked_result = json!({"is_error": true, "error": reason});
(ctx.emit_tool_blocked_fn)(&call, &blocked_result);
output.push(ToolResult::new(call.clone(), blocked_result));

// HookResultControl::Ask deny path  
let blocked_result = json!({"is_error": true, "error": "denied by user"});
(ctx.emit_tool_blocked_fn)(&call, &blocked_result);
output.push(ToolResult::new(call.clone(), blocked_result));
```

### 3. Wire emit function in runtime

```rust
// crates/harnx-runtime/src/tool.rs
fn emit_tool_blocked_with_template(
    call: &ToolCall,
    blocked_result: &Value,
    decl_map: &HashMap<String, ToolDeclaration>,
) {
    let reason = blocked_result
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("blocked by hook")
        .to_string();

    let event = AgentEvent::Tool(ToolEvent::Blocked {
        id: call.id.clone().unwrap_or_default(),
        name: call.name.clone(),
        input: call.arguments.clone(),
        reason,
    });
    let _ = harnx_core::sink::emit_agent_event(event);
}
```

### 4. Render in TUI

```rust
// crates/harnx-tui/src/input.rs
AgentEvent::Tool(ToolEvent::Blocked { id, name, input, reason }) => {
    let body = format!("{}\n⊘ blocked: {}", yaml_input, reason);
    TranscriptItem::ToolCall { name, body, .. }
    // Add to is_turn_boundary match for streaming-assistant index reset
}
```

### 5. Handle in CLI sink

```rust
// crates/harnx/src/cli_event_sink.rs
AgentEvent::Tool(ToolEvent::Blocked { name, reason, .. }) => {
    println!("⊘ {} blocked: {}", name, reason);
}
```

## Why This Works

- **Dedicated variant**: `Blocked` is semantically distinct from `Started` (tool never ran), `Completed` (no result), and `Failed` (execution error during run). Prevents confusion.
- **Emit from blocked branches**: Both hook-block and user-deny paths call `emit_tool_blocked_fn` before returning, guaranteeing visibility.
- **Event-driven architecture**: Adding to `ToolEvent` enum automatically propagates to all consumers (TUI, CLI, future SSE/HTTP servers).
- **YAML input + reason**: TUI shows what was attempted and why it was blocked, debugging-friendly.

## Known Gap

`emit_tool_blocked_with_template` computes `input_rendered` via `render_call()` but `ToolEvent::Blocked` lacks a `markdown: Option<String>` field. The rendered template is discarded. Future improvement: add `markdown` field and pass through to TUI, allowing blocked calls to honor tool `call_template` like `Started` events do.

## Prevention Strategies

**Test Cases:**
- Verify `ToolEvent::Blocked` emitted for hook-blocked calls
- Verify `ToolEvent::Blocked` emitted for user-denied confirmation
- Verify TUI renders blocked calls with `⊘ blocked: <reason>`
- Verify CLI outputs blocked message
- Verify blocked call appears in transcript but not as "in-progress"

**Code Review Checklist:**
- [ ] All rejection/block paths emit appropriate event
- [ ] New `ToolEvent` variants have corresponding TUI/CLI handlers
- [ ] Context struct new fields wired through all call sites

**Pattern:**
When adding a new short-circuit path in tool dispatch, always emit an event before returning. The event stream is the single source of truth for UI consumers.

## Related Issues

- **Plan:** issue-356-blocked-tool-tui
- **Related Solution:** [parallel-tool-dispatch-2026-04-30.md](../parallel-tool-dispatch-2026-04-30.md) — Establishes the "emit after approval" pattern that this solution extends with a dedicated blocked variant
- **Files:** `crates/harnx-core/src/event.rs`, `crates/harnx-engine/src/tool.rs`, `crates/harnx-runtime/src/tool.rs`, `crates/harnx-tui/src/input.rs`, `crates/harnx/src/cli_event_sink.rs`
