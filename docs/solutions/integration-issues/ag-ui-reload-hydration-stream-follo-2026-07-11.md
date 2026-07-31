---
title: "AG-UI session reload: tool summary hydration, live stream follow, and session list sort"
date: 2026-07-11
category: "integration-issues"
problem_type: integration_issue
component: "harnx-serve"
root_cause: "frontend-backend contract gaps across snapshot hydration, promptless SSE lifecycle, and session metadata serialization"
resolution_type: code_fix
severity: high
tags:
  - ag-ui
  - assistant-ui
  - sse
  - snapshot
  - tool-calls
  - hitl
  - reload
  - session-list
plan_ref: "web-ui-bugs"
---

## Problem

Four AG-UI web-chat bugs in `harnx-serve`:

1. Tool summaries lost on session reload — assistant tool calls rendered as `"tool"` with no args or summary
2. Session list unsorted — appeared in arbitrary order
3. Page reload during active run disconnected live stream — tool-approval gate frozen, no resume prompt
4. Sub-agents failing under harnx-serve with missing credentials

## Symptoms

```text
# Bug 1
After reload, tool call cards show toolName="tool" instead of actual name.
No tool args visible. Summary markdown missing.

# Bug 2
Session list order changes arbitrarily. No obvious recency ordering.

# Bug 3
Reload during tool-approval (HITL) → SSE stream closes.
UI shows no pending approval prompt. Gate is lost.
Behavior varies: reload while Running works; reload while Interrupted fails.

# Bug 4
Sub-agent credential errors: "ANTHROPIC_API_KEY not set" or keyring failures.
Same config works in TUI. Fails only under harnx-serve service context.
```

## Root Causes

### 1. Tool summaries — missing tool_calls arrays in snapshot

`@assistant-ui` uses `tool_call_id` to re-attach tool results to their call parts.
Backend `history_messages_for_snapshot` emitted:
- Assistant message with `tool_calls: None`
- Standalone `AgUiMessage::Tool` (no preceding assistant `toolCalls` entry)

Frontend has no tool call part to update, so it creates synthetic part with `toolName: "tool"`.

Proper AG-UI protocol requires:
```json
// Assistant message MUST have tool_calls:
{ "role": "assistant", "content": "...", "tool_calls": [{ "id": "call-abc", "type": "function", "function": { "name": "bash_exec", "arguments": "{\"command\":\"ls\"}" } }] }

// Followed by tool result with matching tool_call_id:
{ "role": "tool", "tool_call_id": "call-abc", "content": "..." }
```

`tool_call_id` MUST be stable across reload. Using random UUIDs breaks re-hydration.

### 2. Session sort — sorted on UUID string, not modified time

`agent_sessions_json` returned sessions in `list_sessions` order (UUID lexical).
Sorted on `id` field, not `modified` timestamp.

Sorting on RFC3339 string has lexical-vs-chronological pitfalls with:
- Variable-length fractional seconds
- Z vs +00:00 timezone suffixes

Correct: sort on `SystemTime`, then format for display.

### 3. Live stream dead after reload — promptless active sessions must follow broadcast

`ag_ui_run_with_call_fn` promptless path (no new prompt) emitted:
```text
RUN_STARTED -> MESSAGES_SNAPSHOT -> (synthetic) RUN_FINISHED → stream closes
```

Ignored the live `tokio::broadcast` receiver entirely.

For IDLE sessions, synthetic `RUN_FINISHED` is correct.
For ACTIVE sessions, MUST attach to live stream instead.

**Critical subtlety:** "Active" = BOTH `SessionState::Running` AND `SessionState::Interrupted`.
`Interrupted` = run paused awaiting tool-approval HITL.
A reload during that window MUST attach to live stream so pending approval prompt reappears.
Otherwise, stream closes and the approval gate is lost.

Additionally: duplicate `RUN_STARTED` events confused frontend run state.
The refactor ensured EXACTLY ONE `RUN_STARTED` at stream head.

### 4. Credential failures — service context env inheritance

Failures occur when service launch context (systemd/container) is missing:

- `HOME` / `XDG_DATA_HOME` / `HARNX_DATA_DIR` → `~/.local/share/harnx/.env` not found
- `*_API_KEY` / `*_TOKEN` vars → credentials not inherited
- `DBUS_SESSION_BUS_ADDRESS` / `XDG_RUNTIME_DIR` → keyring/secret-tool fails

Same config works in TUI because interactive shell has these vars.
Service context often lacks them.

## Solution

### Bug 1: Reconstruct tool_calls arrays with stable IDs

```rust
// crates/harnx-serve/src/ag_ui.rs

/// Derive stable tool_call_id for history hydration.
/// Uses persisted call.id if present; otherwise derives from message_id + index.
fn history_tool_call_id(tool_result: &ToolResult, message_id: MessageId, index: usize) -> ToolCallId {
    if let Some(ref id) = tool_result.call.id {
        ToolCallId::from(id.clone())
    } else {
        ToolCallId::from(format!("{}-tool-{}", message_id, index))
    }
}

// In history_messages_for_snapshot:
// For MessageContent::ToolCalls:
// 1. Build Vec<ToolCall> from persisted tool_results[].call
// 2. Emit assistant message WITH tool_calls: Some(vec)
// 3. Emit tool result messages with matching tool_call_id
```

Assistant message now emitted even if `content: Some("")` when tool calls present.
Frontend can attach results to real tool-call parts.

### Bug 2: Sort on SystemTime, not string

```rust
// crates/harnx-serve/src/lib.rs

/// Comparator for session recency: modified DESC, None last, id-desc tie-break.
/// `Option<SystemTime>` orders `None < Some`, so comparing right-vs-left puts
/// the most-recent `Some` first and `None` last automatically.
fn session_recency_ordering(left: &SessionMeta, right: &SessionMeta) -> Ordering {
    right
        .modified
        .cmp(&left.modified)
        .then_with(|| right.id.cmp(&left.id))
}

// In agent_sessions_json — sessions is a Vec<SessionMeta>, sorted in place:
sessions.sort_by(session_recency_ordering);
```

### Bug 3: Promptless active sessions follow live broadcast

```rust
// crates/harnx-serve/src/ag_ui.rs

/// Active = Running OR Interrupted (awaiting tool-approval HITL)
fn session_state_is_active(state: &SessionState) -> bool {
    matches!(state, SessionState::Running | SessionState::Interrupted(_))
}

// Refactored stream builders:
// - build_live_event_body: frames live broadcast + terminal handling (NO RUN_STARTED)
// - build_prompted_event_stream: RUN_STARTED -> live_event_body
// - build_promptless_event_stream:
//     if is_active: RUN_STARTED -> live_event_body (real terminal closes)
//     else: RUN_STARTED -> MESSAGES_SNAPSHOT -> RUN_FINISHED (synthetic)
```

Key extraction: `build_live_event_body` returns a stream body without leading `RUN_STARTED`.
Both prompted and promptless-active paths prepend exactly ONE `RUN_STARTED`.

### Bug 4: Startup diagnostics

```rust
// crates/harnx-serve/src/bin/harnx-serve.rs

fn log_startup_environment_diagnostics() {
    // Log redacted PRESENCE of credential-related vars (never values):
    for pattern in ["*_API_KEY", "*_TOKEN", "*_SECRET", "*_ACCESS_KEY", "*_KEY"] {
        // Count matches, log count not names/values for sensitive patterns
    }
    // Log resolved paths: HARNX_DATA_DIR, HARNX_ENV_FILE, HOME
    // Log presence of DBUS_SESSION_BUS_ADDRESS, XDG_RUNTIME_DIR
}
```

Added troubleshooting doc: `docs/harnx-serve-subagent-credentials.md`.
FAQ entry: credential symptom table with resolution steps.

## Why This Works

### Tool summaries

Frontend `@assistant-ui` expects `tool_call_id` consistency across assistant message and tool result.
Mismatched/random IDs prevent re-hydration. Stable IDs (persisted or derived from message+index) enable correct attachment.

### Session sort

`SystemTime` comparisons are unambiguous. RFC3339 strings have lexical pitfalls. Sort on typed time before formatting.

### Live follow

`Interrupted` state is a live state for SSE purposes — the run is not finished, just paused.
Synthetic `RUN_FINISHED` closes stream, dropping the approval gate. Following broadcast keeps stream open.

### Credentials

Service context env inheritance is typically the root cause of "works in TUI, fails in server".
Startup diagnostics make the gap visible. Fix is in launch config, not code.

## Prevention Strategies

### Test Cases

```rust
// Tool call ID stability on reload
#[test]
fn history_snapshot_reconstructs_tool_calls_with_matching_ids() {
    // Session with tool_results
    // Snapshot assistant message has tool_calls array
    // tool_calls[].id matches tool result tool_call_id
}

// Active-state live follow
#[test]
fn promptless_join_forwards_live_events_when_session_active() {
    // Actor in Running state
    // Promptless POST subscribes and receives live broadcast
    // No synthetic RUN_FINISHED until real terminal event
}

#[test]
fn promptless_join_forwards_live_events_when_session_interrupted() {
    // Actor in Interrupted(PendingInterruptBatch)
    // Same behavior: attaches to live stream
}

#[test]
fn session_recency_ordering_covers_ties_and_missing_modified() {
    // Distinct modified, equal-modified tiebreak, Some-before-None, both-None tiebreak
}
```

### Code Review Checklist

- [ ] Does snapshot emit assistant `tool_calls` array?
- [ ] Is `tool_call_id` stable (persisted or deterministically derived)?
- [ ] Is session sort on `SystemTime`, not formatted string?
- [ ] Does promptless path check BOTH Running AND Interrupted states?
- [ ] Exactly ONE `RUN_STARTED` at stream head?
- [ ] Are startup diagnostics in place for env vars?

### Best Practices

1. **AG-UI tool_call_id stability**: Persistently store tool call IDs. Derive stable IDs from message+index when missing. Never use random UUIDs.
2. **Sort on typed time**: `SystemTime` comparisons are unambiguous. RFC3339 strings have lexical edge cases.
3. **Active = Running | Interrupted**: Any state where run is not finished must follow live broadcast. Synthetic `RUN_FINISHED` is only for idle.
4. **Service env diagnostics**: Log startup environment snapshots. "Works in TUI, fails in server" is almost always an env inheritance gap.

## Related Issues

- **Plan**: `web-ui-bugs` — detailed notes on all four issues
- **Related Solution**: [ag-ui-tool-approval-interrupt-resume-2026-07-08.md](./ag-ui-tool-approval-interrupt-resume-2026-07-08.md) — Interrupt/resume mechanics for HITL
- **Related Solution**: [ag-ui-server-protocol-integration-2026-07-04.md](./ag-ui-server-protocol-integration-2026-07-04.md) — Initial AG-UI protocol integration
- **Troubleshooting Doc**: `docs/harnx-serve-subagent-credentials.md` — Credential env inheritance

## File Pointers

- `crates/harnx-serve/src/ag_ui.rs`: `history_tool_call_id`, `history_messages_for_snapshot`, `session_state_is_active`, `build_live_event_body`, `build_promptless_event_stream`
- `crates/harnx-serve/src/lib.rs`: `session_recency_ordering`, `agent_sessions_json` sort, `log_startup_environment_diagnostics`
- `crates/harnx-serve/src/ag_ui_tests.rs`: snapshot tool call assertions, active-state stream tests
