---
title: "AG-UI SSE lifecycle guards for late/reconnecting subscribers"
date: 2026-07-12
category: "integration-issues"
problem_type: logic_error
component: "harnx-serve/ag-ui"
root_cause: "per-run lifecycle state mismatched with per-subscriber SSE stream"
resolution_type: code_fix
severity: high
tags:
  - ag-ui
  - sse
  - reconnect
  - lifecycle-events
  - streaming
plan_ref: "issue-1043-ag-ui-lifecycle-guards"
---

## Problem

AG-UI web clients crashed when reconnecting to an in-progress session. The server forwarded lifecycle END events without matching START events in that subscriber's SSE stream.

## Symptoms

- Strict AG-UI clients threw errors on `TEXT_MESSAGE_END`, `STEP_FINISHED`, `TOOL_CALL_END`, or `THINKING_END` without prior START
- Crash occurred during promptless active reconnect (client joins broadcast tail mid-run)
- `MESSAGES_SNAPSHOT` hydrated message history but did not replay open-stream lifecycle state

## Investigation Steps

Traced lifecycle event production vs. subscription boundaries:
1. `AgUiSink` produces lifecycle events per-RUN (one producer)
2. Broadcast channel fans out to multiple SSE subscribers
3. Late subscriber misses START events that already passed
4. Late subscriber still receives END events from ongoing run
5. Client stream becomes inconsistent: END without START

The root insight: `MESSAGES_SNAPSHOT` (sent on connect) hydrates completed content but does NOT signal which lifecycles are currently open.

## Root Cause

Producer-side lifecycle state is per-RUN, but SSE streams are per-SUBSCRIBER. When a client subscribes mid-run, it receives a snapshot of history plus the broadcast tail. The snapshot carries completed events; the broadcast tail carries ongoing events. The framing layer at `build_live_event_body` never saw the START events that happened before subscription, so it could not know that an incoming END event was orphaned.

The broadcast channel does not replay missed events; late subscribers join at the current position.

## Solution

Added `LiveStreamGuard` — a per-SSE-stream state machine in the framing layer. It tracks what THIS stream has forwarded, synthesizing missing START events before orphaned content/END events.

**Key implementation details:**

Guard tracks:
- `started_text_messages: HashSet<MessageId>` — text message starts seen
- `seen_tool_call_ids: Vec<ToolCallId>` — tool call starts seen
- `started_steps: HashSet<String>` — step starts seen
- `thinking_open: bool` — outer ThinkingStart seen
- `thinking_text_open: bool` — inner ThinkingTextMessageStart seen

**Synthesis policy:**

| Event Family | Policy | Rationale |
|---|---|---|
| TextMessage | SYNTHESIZE missing START before content/end | Preserves visible streaming; role hardcoded to Assistant |
| Step | SYNTHESIZE missing START before FINISHED | Preserves UI step indicators |
| Thinking | SYNTHESIZE missing outer ThinkingStart + inner ThinkingTextMessageStart before content/end | Ensures correct nesting: outer before inner |
| ToolCall | DROP unmatched ARGS/END/RESULT | Cannot synthesize START without tool name; snapshot hydrates completed calls |

**Why tool calls are dropped instead of synthesized:**
`ToolCallStart` requires the tool NAME, which is only present in that event. Later events (`ToolCallArgs`, `ToolCallEnd`, `ToolCallResult`) carry only the `ToolCallId`. Without the name, synthesizing a valid START is impossible. The `MESSAGES_SNAPSHOT` already hydrates completed tool calls, so dropping orphaned tool-call tails loses nothing critical.

## Code Structure

```
crates/harnx-serve/src/ag_ui.rs:
  - LiveStreamGuard struct (lines 783-790)
  - frame_guarded_live_event() (lines 792-980)
  - Integration: build_live_event_body -> frame_live_event -> frame_guarded_live_event
```

**Gotcha:** `ToolCallId` in upstream `ag-ui-core` lacks `Hash` trait, so tool tracking uses `Vec<ToolCallId>` with linear membership check. Acceptable for typical stream sizes (<10 tool calls per run). Future: upstream PR to add `Hash`.

## Verification

- `cargo nextest run -p harnx-serve` — 136 tests passed
- `cargo clippy -p harnx-serve --all-targets -D warnings` — clean
- `cargo fmt --check` — clean

Test coverage includes reconnect scenarios for:
- TextMessage: unmatched CONTENT/END synthesizes START
- Step: unmatched FINISHED synthesizes START
- ToolCall: unmatched ARGS/END/RESULT dropped; matched lifecycle forwarded
- Thinking: outer + inner START synthesis on unmatched content/end

## Why This Works

Per-stream guard operates at the framing layer — the only layer that knows what THIS stream has emitted. Upstream `AgUiSink` remains per-run; broadcast semantics unchanged; each subscriber gets self-contained lifecycle repair.

Synthesized events use correct base structure:
```rust
BaseEvent { timestamp: None, raw_event: None }
```

Thinking synthesis ensures nesting: `ThinkingStart` before `ThinkingTextMessageStart` when both are missing.

## Prevention Strategies

**Code Review Checklist:**
- [ ] New event families with lifecycle semantics? Add guard state + synthesis policy
- [ ] Does synthesis require metadata only in START? If yes, drop instead of synthesize
- [ ] Guard state scoped per-stream, not global
- [ ] Test reconnect path for each event family

**Testing:**
- For each lifecycle family, test: late subscriber receives orphaned END → assert synthesized START in output
- Use `decode_sse_bytes_chunks` to verify actual wire format

## Related Issues

- GitHub #1043 — Original crash on reconnect
- `integration-issues/ag-ui-tool-approval-interrupt-resume-2026-07-08.md` — AG-UI interrupt lifecycle support
- `async-patterns/session-actor-concurrency-invariants-2026-07-04.md` — SSE subscription stream patterns
