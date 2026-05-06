---
title: "Session save format consistency between incremental append and full rewrite"
date: 2026-05-05
category: "logic-errors"
problem_type: logic_error
component: "session-persistence"
root_cause: "Format divergence between two write paths (incremental append vs full-rewrite save)"
resolution_type: code_fix
severity: high
tags:
  - session-persistence
  - serialization
  - format-consistency
  - tool-calls
plan_ref: "fix-437-edit-session-corruption"
---

## Problem

Running `.edit session` in harnx followed by exiting the editor (even without changes) corrupted session files containing tool-call history. The session became permanently unreadable with error:

```
error: Invalid log entry in session ...: Tool-role Message entries are no longer supported; use tool_calls/tool_results entries
```

## Symptoms

- Error when loading sessions after `.edit session` operation
- Sessions with tool-call history became corrupted
- Corruption occurred even when editor exited without changes
- No recovery possible without manual file editing

## Investigation Steps

1. Traced `.edit session` flow: calls `save_session()` before opening editor
2. Examined `save()` function in `session.rs` — full-rewrite path serializes ALL messages as `SessionLogEntry::Message`
3. Found loader rejection in `replay_log_entries()`: explicitly rejects `Message` entries with `role: Tool`
4. Identified mismatch: incremental append path writes `ToolCalls` + `ToolResults` pairs, but full-rewrite wrote `Message` for everything
5. Verified: messages with `role: Tool` and `MessageContent::ToolCalls(...)` represent tool-call rounds and MUST be serialized differently

## Root Cause

Session persistence has two write paths:

1. **Incremental append**: writes events one at a time (e.g., `tool_calls`, `tool_results`)
2. **Full-rewrite `save()`**: rewrites entire file (used by `.edit session`, session exit handler)

The `save()` function serialized ALL messages as `SessionLogEntry::Message` entries. But messages with `role: Tool` and `MessageContent::ToolCalls(...)` are the in-memory representation of tool-call rounds — they require serialization as:
1. `SessionLogEntry::ToolCalls { text, thought, calls, timestamp }`
2. `SessionLogEntry::ToolResults { results, timestamp }`

The loader explicitly rejects `Message` entries with `role: Tool`:

```rust
if role == MessageRole::Tool {
    anyhow::bail!(
        "Invalid log entry in session {name}: Tool-role Message entries are no longer supported; use tool_calls/tool_results entries"
    );
}
```

When two separate paths write session data, format divergence between them causes corruption. The full-rewrite path must produce files the loader can read.

## Solution

Added `append_message_entries()` helper in `save()` that detects `role: Tool` + `MessageContent::ToolCalls(...)` messages and emits the correct two-entry format:

```rust
/// Appends YAML log entry/entries for `msg` to `content`.
///
/// Tool-role messages containing `MessageContent::ToolCalls` are split
/// into a `tool_calls` entry (the LLM's request) followed by a
/// `tool_results` entry (the tool outputs), matching the format that
/// `replay_log_entries` expects.
fn append_message_entries(content: &mut String, msg: &Message, session_id: &str) -> Result<()> {
    if msg.role == MessageRole::Tool {
        if let MessageContent::ToolCalls(tc) = &msg.content {
            let calls: Vec<ToolCall> = tc.tool_results.iter().map(|r| r.call.clone()).collect();
            let tool_calls_entry = SessionLogEntry::ToolCalls {
                text: tc.text.clone(),
                thought: tc.thought.clone(),
                calls,
                timestamp: None,
            };
            content.push_str("---\n");
            content.push_str(&serde_yaml::to_string(&tool_calls_entry)?);

            let results: Vec<ToolOutput> = tc.tool_results.iter().map(|r| ToolOutput {
                id: r.call.id.clone(),
                name: r.call.name.clone(),
                output: r.output.clone(),
                switch_agent: r.switch_agent.clone(),
            }).collect();
            let tool_results_entry = SessionLogEntry::ToolResults { results, timestamp: None };
            content.push_str("---\n");
            content.push_str(&serde_yaml::to_string(&tool_results_entry)?);
            return Ok(());
        }
    }

    // Default: write as Message entry
    let entry = SessionLogEntry::Message {
        role: msg.role,
        content: msg.content.clone(),
        timestamp: None,
    };
    content.push_str("---\n");
    content.push_str(&serde_yaml::to_string(&entry)?);
    Ok(())
}
```

Applied this helper in all three message-serialization loops in `save()`:
- Line 760: `session.compressed_messages`
- Line 785: `session.messages` after compress entry
- Line 789: `session.messages` without compress entry

## Why This Works

The `SessionLogEntry` enum has two distinct variants for tool interactions:

1. `ToolCalls` — written immediately after LLM returns, before tool execution
2. `ToolResults` — written after tools complete

This design ensures the transcript is recoverable even if the process crashes mid-execution. The `Message` variant with `role: Tool` is a legacy format that the loader no longer accepts.

By detecting the in-memory representation (`Message { role: Tool, content: MessageContent::ToolCalls(...) }`) and emitting the correct log entries, the full-rewrite path now produces files the loader can read.

## Prevention Strategies

**Test Coverage:**
- Add unit test: save a session with tool-call history, reload it, verify messages match
- Add integration test: `.edit session` on a session with tools, verify no corruption

**Code Review Checklist:**
- [ ] When adding new `SessionLogEntry` variants, update both `save()` and `replay_log_entries()`
- [ ] When modifying serialization logic, ensure both write paths stay in sync
- [ ] Test the full-rewrite path when changing format expectations

**Architecture Consideration:**
- Centralize format knowledge in one place (e.g., a `serialize_message()` function both paths call)
- Consider a "write entry" trait that both append and full-rewrite use

## Related Issues

- **GitHub:** [Issue #437](https://github.com/dobesv/harnx/issues/437) — .edit session corrupts session file
- **Plans:** `fix-437-edit-session-corruption`
