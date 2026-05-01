---
title: "Parallel tool call dispatch with emit ordering safety"
date: 2026-04-30
category: logic-errors
problem_type: logic_error
component: harnx-engine
root_cause: "event emission lifecycle not unified across success/error paths"
resolution_type: code_fix
severity: high
tags:
  - concurrency
  - event-emission
  - tool-dispatch
  - futures
plan_ref: parallel-tool-calls-380
---

## Problem

Tool dispatch changed from sequential loop to two-phase parallel execution. The refactor introduced event emission ordering bugs: `ToolEvent::Started` could be emitted for calls that never received `ToolEvent::Completed`, leaving TUI spinners unresolved. Additionally, fatal error handling discarded partial batch results.

## Symptoms

- TUI showed stuck "in-progress" spinners for blocked, denied, or failed tool calls
- Calls blocked by hooks or denied by user appeared to hang forever in the transcript
- Fatal errors in parallel batches caused loss of successfully completed sibling results
- Users perceived tools as "stuck" despite termination

## Investigation Steps

1. Traced two-phase model: Phase 1 (sequential validation/hooks/confirm) collects approved calls; Phase 2 (parallel) dispatches via `join_all`
2. Found `emit_tool_call_fn` (Started event) called before Block/Ask handling — blocked calls emitted Started but never Completed
3. Found `emit_tool_result_fn` only called on Ok path — Recoverable and Fatal error paths skipped completion emission
4. Identified fatal error path returning `Err(err)` which dropped entire `output` vector

## Root Cause

**Emit ordering gotcha**: Started event emitted before pre-flight decisions. Blocked/denied calls entered "in-progress" UI state but never exited.

**Terminal path asymmetry**: Not all code paths emitting `ToolEvent::Started` had corresponding `ToolEvent::Completed` emission. Error paths (blocked, denied, recoverable, fatal, abort) terminated without signaling completion.

**Fatal error semantics**: Fatal errors captured but `return Err(err)` discarded accumulated successful results from parallel siblings.

## Solution

### 1. Emit Started after approval only

Move `emit_tool_call_fn` call to after Block/Ask handling succeeds:

```rust
// BEFORE: emit before approval check
(ctx.emit_tool_call_fn)(&call, &json_data);  // WRONG
if let HookResultControl::Block { reason } = pre_outcome.control {
    // ...blocked, but Started already emitted
}

// AFTER: emit only for approved calls
if let HookResultControl::Block { reason } = pre_outcome.control {
    output.push(ToolResult::new(call, blocked_result));
    continue;  // No Started emitted
}
// ... Ask handling ...
(ctx.emit_tool_call_fn)(&call, &json_data);  // CORRECT placement
approved.push(ApprovedToolCall { ... });
```

### 2. Emit Completed on all terminal paths

Every terminal path must emit result:

```rust
// Recoverable error path
Err(ToolError::Recoverable(err)) => {
    let error_result = json!({"is_error": true, "error": error_display});
    (ctx.emit_tool_result_fn)(&call, &error_result);  // Added
    output.push(ToolResult::new(call, error_result));
}

// Fatal error path
Err(ToolError::Fatal(err)) => {
    let error_result = json!({"is_error": true, "error": error_display});
    (ctx.emit_tool_result_fn)(&call, &error_result);  // Added
    if fatal_err.is_none() { fatal_err = Some(err); }
}
```

### 3. Use `futures_util` not `futures`

```rust
use futures_util::future::join_all;  // CORRECT - workspace has futures-util
// NOT: use futures::future::join_all;
```

### 4. Preserve order with `join_all`

```rust
let results = join_all(dispatch_futures).await;
for (result, approved_call) in results.into_iter().zip(approved.iter()) {
    // Sequential post-processing in input order
}
```

### 5. Fatal error handling with partial result preservation

```rust
// Collect fatal but process remaining siblings
let mut fatal_err: Option<anyhow::Error> = None;
// ... in result loop ...
Err(ToolError::Fatal(err)) => {
    (ctx.emit_tool_result_fn)(&call, &error_result);
    if fatal_err.is_none() { fatal_err = Some(err); }
    // Do NOT return early inside loop
}
// After loop
if let Some(err) = fatal_err {
    return Err(err);  // Output results already emitted to UI
}
```

## Why This Works

- **Emit after approval**: Only calls that will actually dispatch emit Started, guaranteeing matching Completed
- **Terminal path symmetry**: Every Started has Completed, preventing orphan UI spinners
- **`join_all` + `.zip()`**: Preserves result order despite parallel execution
- **No early return in loop**: All in-flight results post-processed (hooks run, results emitted) before fatal propagates
- **`futures_util`**: Uses existing workspace dependency, avoids duplicate crate

## Prevention Strategies

**Test Cases:**
- `blocked_call_does_not_emit_started`: Verify blocked calls never emit Started
- `recoverable_error_emits_result`: Verify recoverable errors emit Completed
- `fatal_error_emits_result`: Verify fatal errors emit Completed
- Test abort signal during Phase 1 and Phase 2 separately
- Test mixed batch: fast success + slow success + fatal failure

**Code Review Checklist:**
- [ ] Every `emit_tool_call_fn` (Started) has matching `emit_tool_result_fn` (Completed) on all paths
- [ ] Emit functions called AFTER approval decisions, not before
- [ ] Error paths (blocked, denied, recoverable, fatal, abort) all emit completion
- [ ] No early returns inside result processing loops that skip sibling post-processing
- [ ] Parallel dispatch uses `join_all` + `.zip()` for order preservation

**Pattern:**
```text
Phase 1 (sequential): for each call → abort check → parse → hooks → confirm → emit Started → collect
Phase 2 (parallel): join_all approved calls → sequential post-process (emit Completed per result)
```

## Related Issues

- **GitHub:** [#380](https://github.com/example/harnx/issues/380) — Parallel tool call execution
- **File:** `crates/harnx-engine/src/tool.rs`
