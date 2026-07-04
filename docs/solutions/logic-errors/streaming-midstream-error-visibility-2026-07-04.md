---
title: "Fix silent stop on mid-stream streaming LLM error (issue #905)"
date: 2026-07-04
category: "logic-errors"
problem_type: logic_error
component: "streaming-event-handlers"
root_cause: "multi-layer error swallowing"
resolution_type: code_fix
severity: high
tags:
  - streaming
  - error-handling
  - sse
  - tui
  - acp-server
  - multi-layer
plan_ref: "issue-905-stream-error-silent"
---

## Problem

Mid-stream SSE `error` events from Anthropic Claude caused the harness spinner to stop silently—no error message rendered. Users couldn't tell what went wrong.

## Symptoms

- Behavior: Spinner stopped, no output, no error message
- Trigger: Claude API returns `{"type":"error","error":{"type":"api_error","message":"Internal server error"}}` mid-stream
- Frequency: Reproducible when provider returns error after partial text deltas

## Investigation Steps

1. Traced SSE error path: `claude.rs` "error" arm → `catch_error` → `Err(LlmError{status:500,...})`
2. Error correctly propagates up as `Err`, but UI never showed it
3. Audited each layer between source and sink:
   - **TUI**: `crates/harnx-tui/src/prompt.rs:284-293` — on Err with non-empty `text` (including whitespace-only like `"\n"`), returned `Ok` and emitted NO `ModelEvent::Error`
   - **Engine**: `crates/harnx-engine/src/chat_completions.rs:171-180` — used `text.is_empty()` for propagate-vs-swallow decision, inconsistent with non-streaming
   - **ACP server**: `crates/harnx-acp-server/src/lib.rs` — `_ => {}` catch-all dropped `ModelEvent::Error` entirely
4. Identified pattern: error returned correctly from transport, but **each layer independently swallowed or dropped it**

## Root Cause

Multi-layer error swallowing. The mid-stream error correctly became `Err(LlmError{...})` at the client layer, but three separate layers each independently lost it:

1. **TUI streaming wrapper** (`prompt.rs`): `SseHandler::text` only skips truly-empty `""`, so a single whitespace delta before the error counted as "partial content" and triggered swallow-to-Ok without emitting error event.

2. **Engine orchestrator** (`chat_completions.rs`): `text.is_empty()` treated whitespace-only as "has content", returning `Ok` instead of propagating `Err`.

3. **ACP server sink** (`lib.rs`): wildcard match arm `_ => {}` dropped `ModelEvent::Error`, so ACP/headless sessions never surfaced streaming errors.

## Solution

### TUI (`crates/harnx-tui/src/prompt.rs`)

```rust
// Before: silent swallow
Err(err) => {
    if text.is_empty() {
        Err(err)
    } else {
        Ok((text, ...))
    }
}

// After: emit error event, use trim() for empty check
Err(err) => {
    if text.trim().is_empty() {
        Err(err)
    } else {
        emit_agent_event(AgentEvent::Model(ModelEvent::Error(pretty_error_string(&err))));
        Ok((text, ...))
    }
}
```

### Engine (`crates/harnx-engine/src/chat_completions.rs`)

```rust
// Before: whitespace-only counted as "has content"
if text.is_empty() {

// After: whitespace-only is "no meaningful output"
if text.trim().is_empty() {
```

### ACP Server (`crates/harnx-acp-server/src/lib.rs`)

Extracted match into pure function:

```rust
fn event_to_forward(event: AgentEvent, source: Option<AgentSource>) -> Option<AcpForward> {
    match event {
        AgentEvent::Model(ModelEvent::Error(err)) if !err.is_empty() => {
            Some(AcpForward::Text(format!("error: {err}"), source))
        }
        // ... other arms
    }
}
```

Added explicit error arm (previously dropped by `_ => {}`).

## Why This Works

- **TUI**: Whitespace-only partial text no longer triggers silent success. Error event emitted before returning Ok, ensuring visibility.
- **Engine**: `trim().is_empty()` consistent with non-streaming path semantics (propagate error when no meaningful output).
- **ACP**: Structured error event now maps to visible text chunk instead of being dropped. Extracting match avoids cohesion/complexity threshold issues.

Key insight: when an error must reach the user, audit **every layer** between source and sink—a correctly-returned `Err` can still be swallowed independently at multiple hops.

## Prevention Strategies

**Test Cases:**
- `streaming_error_with_empty_text_returns_err` — empty partial → propagate Err
- `streaming_error_with_whitespace_only_text_returns_err` — whitespace-only → propagate Err
- `streaming_error_with_partial_text_returns_ok_and_emits_error` — partial text → Ok + error event
- `acp_chunk_sink_forwards_model_errors_as_visible_text` — ACP forwards error

**Best Practices:**
- Partial-content heuristics for error surfacing should use `text.trim().is_empty()`, not `text.is_empty()`
- Streaming and non-streaming paths should have consistent error-surfacing semantics
- Audit all match arms that drop events (`_ => {}`) — ensure intentional, not oversight
- When adding a new match arm, check CodeScene cohesion/complexity; extracting to a free function restores health

**Code Review Checklist:**
- [ ] Does error handling emit `ModelEvent::Error` before returning partial success?
- [ ] Is the "empty text" check using `trim().is_empty()` for consistency?
- [ ] Are there catch-all match arms dropping relevant events?
- [ ] Do streaming and non-streaming paths handle errors consistently?

**Known Follow-up (non-blocking):**
Forwarding errors as ACP text chunks makes them visible but pollutes `response_text` for nested agents. A dedicated ACP error notification type would be cleaner but requires protocol change.

## Related Issues

- **GitHub Issue:** #905 — "Processing stops with no output on internal server error in streaming LLM response"
- **Related Solution:** [logic-errors/streaming-whitespace-chunk-handling-2026-06-16.md](streaming-whitespace-chunk-handling-2026-06-16.md) — opposite pattern: when preserving whitespace-only chunks is correct
- **Related Solution:** [logic-errors/streaming-final-deduplication-2026-06-09.md](streaming-final-deduplication-2026-06-09.md) — ACP chunk handling patterns
