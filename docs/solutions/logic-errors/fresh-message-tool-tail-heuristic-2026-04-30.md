---
title: "Fresh user messages silently ignored after tool-tail session state"
date: 2026-04-30
category: "logic-errors"
problem_type: logic_error
component: "harnx-runtime/session"
root_cause: "overly-broad heuristic for tool-call continuation detection"
resolution_type: code_fix
severity: high
tags:
  - session-management
  - tool-calls
  - message-handling
  - heuristic-bug
  - interrupt-recovery
plan_ref: "harnx-390-pending-messages-ignored"
---

## Problem

User messages typed while the LLM was working — particularly after a Ctrl-C interrupt or when resuming a session that ended mid-tool-round — were silently ignored instead of being processed as fresh prompts.

## Symptoms

- User types a new message while LLM is processing tool calls
- User presses Ctrl-C to interrupt
- User types a fresh prompt
- Fresh prompt is silently suppressed; session continues as if no input was received
- Resuming a crashed/interrupted session that ended with a `Tool` message also suppresses fresh user input

## Investigation Steps

1. Observed that after Ctrl-C interrupt during tool execution, the session's message list ends with a `Tool` message (the last tool result before interrupt).

2. Traced the message handling through `begin_turn()` and `build_messages()` in `crates/harnx-runtime/src/config/session.rs`.

3. Found both functions used an overly-broad heuristic to detect "tool-call continuation" rounds:
   ```rust
   let is_continuation = session.messages.last().is_some_and(|m| m.role == MessageRole::Tool);
   ```

4. This single condition only checked whether the last message was a `Tool` message, but didn't distinguish between:
   - Genuine tool continuations (agent loop calling `merge_tool_results()`)
   - Fresh user prompts arriving after session state coincidentally matched the pattern

5. Reviewed PR #361 (Bash tool inconsistencies fixups) which introduced the session-tail-Tool check to prevent duplicate user messages during multi-round tool loops. The single condition was correct for that use case but too broad.

## Root Cause

The continuation detection logic in `begin_turn()` and `build_messages()` used a single-condition heuristic:

```rust
// TOO BROAD — matches fresh prompts after interrupted tool rounds
let is_continuation = session.messages.last().is_some_and(|m| m.role == MessageRole::Tool);
```

After an interrupt or session resume, the session might legitimately end with a `Tool` message even though the user is sending a **fresh** new prompt, not continuing a tool loop. The heuristic incorrectly classified these as continuations, causing fresh user messages to be suppressed.

The missing signal: genuine tool continuations are always initiated by `merge_tool_results()` in the agent loop, which sets `input.tool_calls = Some(...)` on the input. Fresh user prompts never have `tool_calls` set.

## Solution

Added a second condition requiring `input.tool_calls.is_some()` to confirm genuine tool continuation:

```rust
fn is_tool_continuation(input: &Input, messages: &[Message]) -> bool {
    input.tool_calls.is_some()
        && messages
            .last()
            .is_some_and(|m| m.role == MessageRole::Tool)
}
```

Extracted the heuristic into a private helper `is_tool_continuation(input, messages)` used in both `begin_turn()` and `build_messages()` to avoid duplication and ensure consistency.

**Before (in both functions):**
```rust
let is_continuation = session.messages.last().is_some_and(|m| m.role == MessageRole::Tool);
if is_continuation {
    // skip adding user message
}
```

**After:**
```rust
if is_tool_continuation(input, &session.messages) {
    // skip adding user message — genuine tool continuation
}
```

## Why This Works

The dual condition correctly distinguishes:

1. **Genuine tool continuations**: `tool_calls.is_some()` AND session ends with `Tool` message. These are mid-tool-loop iterations where the agent called `merge_tool_results()` to accumulate tool context. User message already present from original turn; skipping prevents duplicates.

2. **Fresh prompts after tool-tail**: `tool_calls.is_none()` despite session ending with `Tool`. These are new user inputs after interrupts or session resumes. The `tool_calls` field is only populated by `merge_tool_results()` during active tool loops; fresh prompts never have it set.

The `tool_calls` field serves as the authoritative signal that the agent loop is actively managing a tool continuation, eliminating false positives from coincidental session state.

## Prevention Strategies

**Test Cases:**
- `fresh_message_after_tool_tail_is_included_in_build_messages` — verifies `build_messages` includes fresh user message when `tool_calls == None`
- `fresh_message_after_tool_tail_is_saved_by_begin_turn` — verifies `begin_turn` persists fresh user message
- `fresh_message_after_orphan_repair_is_included_in_build_messages` — verifies behavior for crash-interrupted sessions repaired on reload

**Code Review Checklist:**
- [ ] Do heuristics with multiple valid states use all necessary conditions?
- [ ] Are edge cases (interrupts, crashes, resumes) considered in state-dependent logic?
- [ ] Are continuation/interruption patterns tested explicitly?

**Design Principle:**
When a single-condition heuristic matches multiple semantically different states, add distinguishing signals. Coincidental state overlap is a common source of logic bugs in state machines.

## Related Issues

- **Issue:** [#390](https://github.com/example/harnx/issues/390) — Pending messages ignored after tool rounds
- **PR #361:** Introduced the session-tail-Tool check to prevent duplicate user messages; single condition was correct for that use case but didn't account for fresh prompts after coincidental tool-tail state
