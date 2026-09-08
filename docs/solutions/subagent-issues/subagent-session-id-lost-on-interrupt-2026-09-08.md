---
title: "Sub-agent session_id lost on parent-side interruption"
date: 2026-09-08
last_verified: 2026-09-08
component: "harnx-runtime/src/nats_worker/subagent_toolset.rs"
problem_type: logic_error
status: current
anchors:
  - crates/harnx-core/src/session.rs:108-118
  - crates/harnx-runtime/src/config/session.rs:227-245
  - crates/harnx-runtime/src/nats_tool_provider.rs:376
tags:
  - subagent
  - session-id
  - cancellation
  - tool-result
  - transcript
plan_ref: "subagent-session-id-lost-on-interrupt"
---

# Sub-agent session_id lost on parent-side interruption

## When this is relevant

Adding error handling, resumption, or monitoring for cancelled/interrupted sub-agent
delegations. If you expect the child `session_id` to always be visible to the parent agent
via error text or tool output, check which abort path applies.

Symptoms:
- Parent agent cannot resume or inspect a sub-agent session after Ctrl+C
- Error messages sometimes include `session_id`, sometimes don't
- TUI shows sub-agent header with session_id but parent LLM never sees it

## Durable lesson

Two distinct abort paths exist for sub-agent tool calls, and only one can embed the
child session_id in a tool result/error the parent reads:

**Path A — worker-side abort** (`termination.rs`): Parent times out, budget exhausted, or
mid-run error. Worker constructs a `ToolInvokeError` reply that parent receives — error
string CAN be enriched with `child_session_id` (done as of #1604).

**Path B — parent-side abort** (`nats_tool_provider.rs:376`): User hits Ctrl+C. Parent
returns `Fatal("tool call aborted")` WITHOUT reading the worker reply. Worker's reply
(if any) is dropped. `child_session_id` is NOT in scope at this point. Error-string
enrichment cannot fix this path.

Solution: a **durable entry** (`SubAgentStarted`) appended to the parent session log at
delegation start. Reconstruction renders it as a User "[Runtime note]" message AFTER the
tool result, so the parent LLM sees the child session_id on the next turn regardless of
abort path. This preserves tool_use→tool_result adjacency.

The durable append is best-effort: if it fails (e.g. a transient JetStream error), the
worker logs a warning and still runs the delegation rather than aborting it at the start.
In that rare case the child id falls back to the live advisory event and the enriched
error/tool-result strings. So recovery is guaranteed only when the durable append
succeeds, which is the normal path.

## Evidence and current anchors

- `SessionLogEntry::SubAgentStarted` — durable entry in parent log (`session.rs:108-118`)
- Reconstruction queues it after tool results (`config/session.rs:227-245`)
- Parent-side abort returns Fatal without reading reply (`nats_tool_provider.rs:376`)
- Worker-side errors enriched with session_id (`termination.rs:100-103`, `termination.rs:232-234`)

## Ordering invariant for transcript reconstruction

`SubAgentStarted` arrives between `ToolCalls` and `ToolResults`. Reconstruction queues it in
`messages_queued_during_tool` so the transcript order becomes:

```text
ToolCalls → ToolResults → User([Runtime note] Started sub-agent...)
```

This preserves the tool_use→tool_result adjacency required by LLM APIs. Any new mid-tool
entry type must follow the same pattern or transcripts will be invalid.

## Failed approaches or trade-offs

- **Enrich error strings only:** Fixes Path A but not Path B (parent drops reply before
  reading it).
- **Parent-side call_id→child_id map:** Would require tracking in-flight delegations in
  `NatsToolProvider`, but the map entry would be created by the same code path that
  isn't reached on parent abort. Durable entry is simpler and works for TUI reconnects.
