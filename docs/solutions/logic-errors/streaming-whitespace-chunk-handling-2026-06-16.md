---
title: "Preserve whitespace-only chunks in streaming deltas"
date: 2026-06-16
category: "logic-errors"
problem_type: logic_error
component: "harnx-acp-client"
root_cause: "incorrect empty-string guard trimming whitespace before filtering"
resolution_type: code_fix
severity: medium
tags:
  - streaming
  - whitespace
  - markdown
  - acp
  - chunks
plan_ref: "issue-862-whitespace-streaming"
---

## Problem

Streamed markdown numbered lists rendered run-together in the TUI, e.g. "1. Confirm2. Confirm3. Reject...Key practical call:" with no line breaks between items.

## Symptoms

- TUI displayed concatenated list items during streaming: "1. Item1. Item2. Item"
- Issue #862: Is whitespace being stripped in streaming output?
- Only affected ACP sub-agent streaming through the `AcpNotificationClient` receive path
- ACP server send-side and CLI sink rendered correctly

## Investigation Steps

1. Reproduced by running a sub-agent that streams numbered lists — items concatenated without separators
2. Traced streaming path: `AcpNotificationClient::session_notification` handles `AgentMessageChunk` and `AgentThoughtChunk`
3. Found the guard: `if text.trim().is_empty() { None }` — discards chunks where trimmed text is empty
4. Identified that ACP sub-agents stream newline/space separators as standalone whitespace-only chunks
5. Compared with ACP server (`harnx-acp-server/src/lib.rs`) and CLI sink (`harnx/src/cli_event_sink.rs`) — both used `is_empty()` not `trim().is_empty()`
6. Root cause isolated to client receive-side only

## Root Cause

The `AcpNotificationClient::session_notification` method filtered streaming chunks with:

```rust
if text.trim().is_empty() { None }
```

ACP sub-agents stream the newline/space separators between list items as standalone whitespace-only content chunks (e.g., a single `"\n"`). The `trim().is_empty()` guard discarded these chunks entirely, causing adjacent text chunks to concatenate with no separator. This also corrupted `response_text` accumulated for parent agent next-turn input.

## Solution

Changed the guard to preserve whitespace-only chunks:

**Before:**
```rust
if text.trim().is_empty() { None }
```

**After:**
```rust
if text.is_empty() { None }
```

File: `crates/harnx-acp/src/client.rs` in `AcpNotificationClient::session_notification`

The same anti-pattern existed in the TUI thought-chunk handler
(`crates/harnx-tui/src/input.rs`, the `ModelEvent::ThoughtChunk` arm): after
stripping `<think>`/`</think>` tags it used `clean.trim().is_empty()`, which
dropped whitespace-only thought chunks and concatenated streamed thought lines.
Changed to `clean.is_empty()` so pure-tag/empty chunks are still skipped while
whitespace separators are preserved. (`flush_pending_thought` still `.trim()`s
the fully accumulated thought, so leading/trailing whitespace is not shown.)

This matches the existing behavior in:
- ACP server send-side (`harnx-acp-server/src/lib.rs`)
- CLI event sink (`harnx/src/cli_event_sink.rs`)
- TUI message-chunk handler (`harnx-tui/src/input.rs`, already `is_empty()`)

## Why This Works

When filtering streaming deltas/chunks, only genuinely empty chunks (zero-length strings) should be dropped. Whitespace-only chunks (lone `"\n"`, `" "`, etc.) carry semantically significant inter-token whitespace for markdown rendering and text reconstruction.

Using `is_empty()` checks for zero-length content. Using `trim().is_empty()` incorrectly treats `" "` and `"\n"` as "empty" when they are structural separators in markdown.

## Prevention Strategies

**Test Cases:**
- `whitespace_only_message_chunk_is_forwarded_not_stripped` — verifies lone newline preserved
- `empty_message_chunk_is_dropped` — verifies genuinely empty chunk filtered out

**Best Practices:**
- When filtering streaming deltas, use `text.is_empty()` not `text.trim().is_empty()`
- Inter-token whitespace (newlines, spaces) is semantically significant in streamed markdown
- Audit all chunk-filtering guards for this pattern

**Code Review Checklist:**
- [ ] Does streaming chunk filter use `is_empty()` not `trim().is_empty()`?
- [ ] Are there tests for whitespace-only chunk handling?
- [ ] Is filtering behavior consistent across send-side and receive-side?

## Related Issues

- **Issue:** #862 — "Is whitespace being stripped in streaming output?"
- **Related Solution:** [logic-errors/streaming-final-deduplication-2026-06-09.md](streaming-final-deduplication-2026-06-09.md) — ACP chunk handling for Final vs streamed text
