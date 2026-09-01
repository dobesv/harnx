---
title: "Sub-agent TUI reply hidden by sub_agent_progress early-return trap"
date: 2026-09-01
last_verified: 2026-09-01
component: "harnx-tui/src/subagent_sessions.rs"
problem_type: logic_error
status: current
anchors:
  - crates/harnx-tui/src/subagent_sessions.rs:257-274
  - crates/harnx-runtime/src/nats_worker/subagent_toolset.rs:285-290
tags:
  - subagent
  - TUI
  - tool-result
  - early-return
  - progress-events
plan_ref: "subagent-tui-reply"
---

# Sub-agent TUI reply hidden by sub_agent_progress early-return trap

## When this is relevant

Adding logic that runs on `*_session_prompt` tool completion in the TUI live-event path. If your new code is placed after a `subagent_progress_from_output` check, it will never execute in production.

Symptoms:
- Sub-agent reply visible in durable-history tests but not in live TUI sessions
- Reply row appears in tests with synthetic payloads that omit `sub_agent_progress`
- Reply row missing in production for `*_session_prompt` completions

## Durable lesson

Production `*_session_prompt` tool results (built by `turn_result_value` in `subagent_toolset.rs:285-290`) **always contain both** `response` and `sub_agent_progress`. The progress field is not optional in the canonical payload.

In `handle_subagent_marker` (`subagent_sessions.rs:257`), checking `subagent_progress_from_output` first and returning early will skip any reply-handling logic placed after it. A synthetic test payload that omits `sub_agent_progress` will pass, masking the dead code.

Reply handling must run **before** the progress dispatch:

```rust
// CORRECT order — reply first, then progress/completed dispatch
AgentEvent::Tool(ToolEvent::Completed { output, .. }) => {
    let progress = subagent_progress_from_output(output);
    let Some(key) = subagent_key_from_output(output, &cluster) else {
        // handle progress-only case
    };

    self.insert_subagent_reply(parent, &key, output);  // ← first

    match progress {
        Some(p) => self.record_subagent_progress(parent, p),
        None => self.record_subagent_completed(parent, key),
    }
    true
}
```

## Evidence and current anchors

- `crates/harnx-runtime/src/nats_worker/subagent_toolset.rs:285-290` — production tool result construction:
  ```rust
  Ok(json!({
      "session_id": ...,
      "response": response,           // ← always present
      "sub_agent": source,
      "sub_agent_progress": completed.progress,  // ← always present
  }))
  ```
- `crates/harnx-tui/src/subagent_sessions.rs:257-274` — fixed `ToolEvent::Completed` arm now inserts reply before progress dispatch
- `crates/harnx-tui/src/lifecycle.rs:743-752` — durable-load path extracts `response` via `subagent_reply_item_from_output`
- Plan note `f665af36` — first fix was dead code; synthetic test payload omitted `sub_agent_progress`

## Related constraint: single source of truth for reply

Child model/streaming events are forwarded to the parent only as progress metrics (via `ProgressEventSink` in `subagent_progress.rs:47-62`) and to a separate fullscreen monitored-session view. They never reach the parent main transcript. The tool result's `output["response"]` is the **only path** the reply reaches the parent main transcript.

## Ordering invariant for detail-view pairing

`ToolResultMarkdown` is non-navigable (`types.rs:443-457`) and must sit at the position immediately following its `ToolCall` for `detail_view.rs:append_paired_tool_result` to show the paired result. Hence transcript order for a sub-agent delegation is:

```
ToolCall(prompt) → ToolResultMarkdown(reply) → SubAgentSession(status)
```

The reply row inserted before the navigable `SubAgentSession` status row.

## Failed approaches or trade-offs

- **Embedding reply inside SubAgentSession row with expand/collapse:** Would require new enum-field ripple across ~8 files and a new expand/collapse interaction model. Rejected in favor of reusing the mature `ToolResultMarkdown` render/copy/detail pipeline.
