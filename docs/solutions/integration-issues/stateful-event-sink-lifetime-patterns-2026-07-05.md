---
title: "Stateful event sink lifetime patterns and test-harness pitfalls"
date: 2026-07-05
category: integration-issues
problem_type: integration_issue
component: harnx-serve
root_cause: "per-event sink recreation and error/success emission asymmetry"
resolution_type: code_fix
severity: high
tags:
  - event-sink
  - state-machine
  - tokio
  - sse
  - test-harness
  - ag-ui
  - broadcast
  - emit-symmetry
plan_ref: harnx-session-refactor
last_updated: 2026-07-05
---

## Problem

Multiple integration-level bugs in the AG-UI SSE event mapping layer where stateful sink state was incorrectly reset per event, recoverable tool errors were persisted but never emitted, and test helpers masked failures with silent empty returns.

## Symptoms

- **Stateful sink reset**: Live SSE streams emitted duplicate `THINKING_START` events and never emitted `THINKING_END`/`THINKING_TEXT_MESSAGE_END` — reasoning UI showed broken/flickering state
- **Missing tool results**: Recoverable tool errors (e.g., "session has not been saved yet") left tool calls unterminated on SSE stream — clients saw `TOOL_CALL_START` + `TOOL_CALL_ARGS` but no `TOOL_CALL_END`/`TOOL_CALL_RESULT`
- **Test masking**: SSE test helper returned `events: []` on timeout, hiding real stream behavior and making diagnosis impossible
- **Unit tests passed**: Sink unit tests drove the sink directly and missed the integration-level lifetime bug

## Investigation Steps

1. **Stateful sink bug** (C4): Unit tests passed with exact thinking event ordering, but live integration test showed duplicate starts and missing ends. Traced to `BroadcastEventSender::emit` creating a fresh `AgUiSink` per event via throwaway relay channel — resetting `in_thinking_segment` and `text_message_started` atomics on every call.

2. **Tool result emission** (C3): SSE criterion_10 returned `events: []`. Initial assumption: test helper timing issue. Probe showed real events on wire but missing `TOOL_CALL_END`. Traced to `harnx-engine/src/tool.rs` recoverable-error branch: it persisted `ToolResult` but never called `emit_tool_result_fn`, unlike the success branch.

3. **Test harness masking**: The SSE read helper used `unwrap_or_default()` on outer timeout, manufacturing empty `events: []` and hiding real stream state. Hardened helper to panic with collected partial state.

4. **Parallel delegation collision**: During C2 proof, T2 was mid-refactor on the runtime API. Running both agents concurrently caused transient broken build. A stale agent session (context predating committed changes) accidentally reverted the working_dir refactor.

## Root Cause

### 1. Stateful Sink Per-Event Recreation

```rust
// BROKEN: BroadcastEventSender::emit created fresh sink per event
fn emit(&self, event: AgentEvent, source: Option<AgentSource>) {
    let (tx, rx) = mpsc::unbounded_channel(); // throwaway relay
    let sink = AgUiSink::new(tx, self.message_id.clone()); // FRESH sink every call
    sink.emit(event, source);
    // relay to broadcast...
}
```

Stateless mappings (text deltas, tool starts) survived because each event maps independently. The stateful thinking state machine (`in_thinking_segment`, `text_message_started`) reset on every event, breaking:
- `THINKING_START` re-emitted for every `ThoughtChunk`
- `THINKING_END` never emitted because `swap(false)` always found segment "closed"

### 2. Error/Success Emission Asymmetry

```rust
// harnx-engine/src/tool.rs
// Success branch (line 212):
(ctx.emit_tool_result_fn)(&call, &result); // EMIT
output.push(ToolResult::new(call, result));

// Recoverable error branch (OLD, line 223-238):
let error_result = json!({"is_error": true, "error": error_display});
output.push(ToolResult::new(call, error_result)); // NO EMIT!
```


### 3. Test Helper Silent Failure

```rust
// BROKEN: Manufacturing empty on timeout
tokio::time::timeout(timeout, fut).await.unwrap_or_default()

// FIXED: Fail loudly with partial state
tokio::time::timeout(timeout, fut).await
    .expect("SSE read should finish before outer timeout")
```

## Solution

### 1. Persistent Sink Per Run

`BroadcastEventSender` now owns one `AgUiSink` for the run's lifetime:

```rust
// crates/harnx-serve/src/session_actor.rs
struct BroadcastEventSender {
    sink: AgUiSink, // Persistent, not per-event
}

impl BroadcastEventSender {
    fn new(tx: broadcast::Sender<Event>, message_id: MessageId, 
           history_snapshot: Arc<dyn Fn() -> Vec<AgUiMessage> + Send + Sync>) -> Self {
        Self {
            sink: AgUiSink::new_broadcast_with_snapshot(tx, message_id, history_snapshot),
        }
    }
}

impl AgentEventSink for BroadcastEventSender {
    fn emit(&self, event: AgentEvent, source: Option<AgentSource>) {
        self.sink.emit(event, source); // Delegate, don't recreate
    }
}
```

### 2. Emit Symmetry for Recoverable Errors

```rust
// crates/harnx-engine/src/tool.rs:238
let error_result = json!({"is_error": true, "error": error_display});
(ctx.emit_tool_result_fn)(&call, &error_result); // NOW EMITS
output.push(ToolResult::new(call, error_result));
```

### 3. Loud Test Helpers

Test SSE reads must panic on timeout/EOF with partial collected state:

```rust
tokio::time::timeout(timeout, fut)
    .expect("SSE read should finish before outer timeout")
```

Never use `unwrap_or_default()` on stream boundaries — it manufactures empty data and hides root cause.

## Why This Works

- **Persistent sink**: Atomic state (`in_thinking_segment`, `text_message_started`, `turn_counter`) survives across `emit()` calls within a run. State machine transitions work as designed.
- **Emission symmetry**: All non-fatal tool outcomes (success and recoverable error) emit exactly once. Fatal errors intentionally silent at this layer — loop aborts.
- **Loud helpers**: Failures surface as panics with partial context, not silent empty returns. Diagnosis becomes trivial.

## Prevention Strategies

### Test Cases

- **Integration test for stateful behavior**: Sink unit tests alone cannot catch lifetime bugs. Use live broadcast channel in test, not just `UnboundedSender`. Assert exact thinking event counts (1 start, 1 end).
- **Recoverable error emission**: Add engine test asserting one emit for recoverable error path, matching success path.
- **SSE helper assertion**: Assert that helpers panic on timeout, not return empty.

### Code Review Checklist

- [ ] Does stateful sink have run-scoped lifetime (not per-event)?
- [ ] Do all result-producing branches emit to sinks (success AND error)?
- [ ] Do test helpers fail loudly on timeout/EOF, not silently return empty?
- [ ] Are parallel agents working on overlapping files sequenced?

### Integration Test Pattern

For any stateful event sink:

```rust
// Test with REAL broadcast channel, not unit-test stub
let (tx, mut rx) = broadcast::channel(16);
let sink = BroadcastEventSender::new(tx, message_id, snapshot);
// Emit multiple events
sink.emit(thought_chunk, None);
sink.emit(thought_chunk, None);
sink.emit(text_chunk, None);
// Assert: one THINKING_START, one THINKING_END, two CONTENT
```

## Related Issues

- **Plan note**: `c4-fail-stateless-sink` — initial C4 failure exposing the lifetime bug
- **Plan note**: `c3-engine-fix-verified` — engine-level emit fix verification
- **Plan note**: `418fe1c6` — test harness masking issue
- **Related Solution**: [async-patterns/session-actor-concurrency-invariants-2026-07-04.md](../async-patterns/session-actor-concurrency-invariants-2026-07-04.md) — Phase 2 actor concurrency patterns
