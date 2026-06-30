---
title: "Fix one-sided NATS attach/resume transcript by adding AgentEvent::User"
date: 2026-06-29
category: integration-issues
problem_type: integration_issue
component: "harnx-runtime, harnx-core, harnx-tui, harnx-acp-server, harnx"
root_cause: "AgentEvent protocol had no user-message variant, so replay rendered only assistant messages"
resolution_type: code_fix
severity: medium
tags:
  - agent-event-sink
  - nats
  - attach-resume
  - transcript
  - event-protocol
plan_ref: issue-916-attach-transcript
---

## Problem

Remote attach/resume transcripts showed only assistant messages. User turns were missing because `AgentEvent` had no variant to carry user text from the runtime to sinks.

## Symptoms

- Resumed remote sessions displayed one-sided transcripts (assistant-only).
- `render_log_entry_to_sink` in `nats_client_session.rs` rendered user messages as no-ops.
- No event type existed to represent user input in the replay path.

## Investigation Steps

1. Traced replay path: `replay_history_to_sink` → `render_log_entry_to_sink` → matched `SessionLogEntry::Message` but had no way to emit user role.
2. Checked `AgentEvent` enum — only `Model`, `Tool`, `Turn`, `Session`, `Notice`, `Status`, `Plan` variants. No `User`.
3. Audited sink implementations (`harnx-tui`, `harnx-acp-server`, `harnx`) — all used catch-all `_ => {}` matches that silently dropped unknown events.
4. Identified dedup hazard: current turn's user message is both appended to the NATS log AND locally echoed by the TUI on submit. Naive replay duplicates it.

## Root Cause

**Protocol gap**: `AgentEvent` (crates/harnx-core/src/event.rs) lacked a user-message variant. The event protocol is the single source of truth for frontend rendering; without `User` events, replay could not render user turns.

**Catch-all trap**: All `AgentEventSink::emit()` implementations used `_ => {}` fallbacks. The Rust compiler does not warn when new enum variants are added, so missing handlers silently drop events.

## Solution

### 1. Add `UserEvent` to the protocol

In `crates/harnx-core/src/event.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UserEvent {
    Message { content: String },
}

// In AgentEvent:
pub enum AgentEvent {
    // ... existing variants
    User(UserEvent),
}
```

### 2. Handle `AgentEvent::User` in every sink

**TUI** — `crates/harnx-tui/src/input.rs`, `render_agent_event()`:
```rust
AgentEvent::User(UserEvent::Message { content }) => {
    vec![TranscriptItem::UserText(content.clone())]
}
```

**ACP server** — `crates/harnx-acp-server/src/lib.rs`, `AcpChunkSink::emit()`:
```rust
AgentEvent::User(UserEvent::Message { content }) => {
    if !content.is_empty() {
        forward(AcpForward::Text(content));
    }
}
```

**CLI** — `crates/harnx/src/cli_event_sink.rs`:
- Added `UserEvent` to imports.
- Added match arm: cleanup spinner, print dimmed user text via `eprintln!`.
- Added to `is_model_output_event()` (marks message-bearing events).

### 3. Render user entries in history replay

In `crates/harnx-runtime/src/nats_client_session.rs`, `render_message_entry()`:
```rust
match role {
    MessageRole::User => {
        sink.emit(AgentEvent::User(UserEvent::Message { content: text }), None);
    }
    MessageRole::Assistant => { /* existing assistant handling */ }
}
```

### 4. Dedup current-turn user message by exact seq

The current turn's user message is appended to the durable log BEFORE attach/replay. The TUI also locally echoes it on submit. Naive replay duplicates.

**Correct approach**: Skip the entry whose `seq == user_msg_seq` (exact seq returned from `append_event_async`).

```rust
fn should_skip_replay_entry(seq: u64, user_msg_seq: u64) -> bool {
    seq == user_msg_seq
}

fn replay_history_to_sink(
    effective_history: &[(u64, SessionLogEntry)],
    user_msg_seq: u64,
    sink: Arc<dyn AgentEventSink>,
) {
    for (seq, entry) in effective_history {
        if should_skip_replay_entry(*seq, user_msg_seq) {
            continue;
        }
        render_log_entry_to_sink(entry, sink.clone());
    }
}
```

**Why not reverse-search?** A concurrent writer could append another user message between the append and replay, causing a reverse-search to skip the wrong message. Exact-seq comparison is race-safe.

### 5. Robust mutation handling

On mutation-reconstruction failure, fall back to `history.to_vec()` (raw log) rather than an empty vec. Don't blank the transcript.

## Why This Works

- **Protocol completeness**: `AgentEvent::User(UserEvent::Message)` gives replay a carrier for user text.
- **Sink coverage**: Every `AgentEventSink` implementation now handles `User` events. No silent drops.
- **Exact-seq dedup**: The current turn's user message is skipped by precise seq, avoiding both duplication and race conditions.
- **Fallback preserves history**: Mutation failures no longer result in empty transcripts.

## Prevention Strategies

**When adding an `AgentEvent` variant:**

1. **Audit all sinks**. The compiler will not flag missing handlers due to catch-all `_ => {}`.
2. For each sink file:
   - `crates/harnx-tui/src/input.rs` — `render_agent_event()`
   - `crates/harnx-acp-server/src/lib.rs` — `AcpChunkSink::emit()`
   - `crates/harnx/src/cli_event_sink.rs` — `CliAgentEventSink::emit()` + `is_model_output_event()` if message-bearing
3. Add a serde round-trip test if the event crosses the NATS wire.
4. Add per-sink rendering tests.

**Dedup patterns:**

- Use exact sequence IDs, not reverse-search, when skipping entries during replay.
- Document the race condition rationale in code comments.

**Testing:**

- Exercise production replay helper directly in tests (don't duplicate loop logic).
- Verify which entry is skipped by seq, not by position.

**Code Review Checklist:**

- [ ] New `AgentEvent` variant handled in TUI `render_agent_event()`?
- [ ] New `AgentEvent` variant handled in ACP `AcpChunkSink::emit()`?
- [ ] New `AgentEvent` variant handled in CLI `CliAgentEventSink::emit()`?
- [ ] If message-bearing: added to CLI `is_model_output_event()`?
- [ ] Serde round-trip test for wire-crossing events?
- [ ] Replay dedup uses exact seq, not reverse-search?

## Related Issues

- **Issue:** [#916](https://github.com/dobesv/harnx/issues/916) — NATS attach/resume renders one-sided transcript
- **Prior Solution:** [tui-compaction-spinner-corruption-2026-06-08.md](./tui-compaction-spinner-corruption-2026-06-08.md) — AgentEvent sink pattern and catch-all trap
- **Prior Solution:** [mcp-tool-template-acp-propagation-2026-04-30.md](./mcp-tool-template-acp-propagation-2026-04-30.md) — AcpChunkSink event handling
