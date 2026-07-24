---
title: "Surface background task failures in title generation (silent API key error)"
date: 2026-07-23
category: "logic-errors"
problem_type: logic_error
component: "harnx-runtime, harnx-core, harnx-tui"
root_cause: "Silently swallowed error in spawned background task — warn! log invisible in TUI"
resolution_type: code_fix
severity: medium
tags:
  - background-task
  - event-sink
  - title-generation
  - error-handling
  - NullSink
  - diagnostics
plan_ref: "terminal-title-update"
---

## Problem

Configured title generation never updated terminal title. Static plumbing verified correct (config → agent file → runtime → `maybe_generate_title` → `SessionEvent::TitleUpdated` → crossterm `SetTitle`). Root cause: title agent's API call failed with `Miss 'api_key'`, but failure was logged via `warn!(...)` — invisible to TUI users. No visible error, no title, no diagnostics.

## Symptoms

- Terminal title remains unchanged despite correct `title_agent` config
- No error message visible in TUI, CLI, or web frontend
- `harnx info agent title-agent` resolves correctly, model valid
- Tmux/mock-server E2E with mock LLM: title pipeline works when credentials present
- Log file shows: `[WARN] Failed to generate session title: Failed to call chat-completions api (client: gemini, model: gemini:gemini-3.1-flash-lite)`

## Investigation Steps

1. Static traced plumbing: `Config::run_post_turn_maintenance` → `maybe_generate_title` → `claim_titling` → `resolve_title_agent` → `generate_title` → `handle_title_result` → `emit_agent_event(TitleUpdated)`. All present.
2. Suspected `SetTitle` lost under ratatui alternate-screen/raw-mode — ruled out via tmux E2E: title updates observed under raw mode, alternate screen, and after redraws.
3. Built mock-LLM E2E: normal turn + title-model request → mock returns title → pane title updated within 1s. Pipeline functional.
4. Reproduced failure: same flow with unauthenticated Gemini client. Main turn succeeded; title generation failed with `Miss 'api_key'`.
5. Found: `handle_title_result` called `warn!("Failed...")` but emitted NO event. User saw nothing.

**Key insight**: When all static plumbing is correct but a feature "does nothing," suspect silently swallowed errors in spawned/background tasks.

## Root Cause

Two-part bug:

1. **Invisible failure**: `handle_title_result` logged error with `warn!` but never emitted a `SessionEvent`. TUI has no access to log crate output unless explicit event path fires.

2. **Event pollution**: Background title generation runs the title agent through normal chat path, leaking model/streaming/retry events into main transcript. Observed in E2E: title-agent completions rendered in user-visible output.

```rust
// Before (in session_ops_title.rs)
fn handle_title_result(result: anyhow::Result<Option<String>>) {
    match result {
        Ok(Some(title)) => { /* emit TitleUpdated */ }
        Ok(None) => {}
        Err(err) => {
            warn!("Failed to generate session title: {err}");
            // Nothing else — user never sees this
        }
    }
}
```

## Solution

### Fix A — Surface background failures with dedicated event

Added `SessionEvent::TitleGenerationFailed(String)` (mirrors existing `CompactingFailed`):

```rust
// In harnx-core/src/event.rs
pub enum SessionEvent {
    // ... existing variants ...
    TitleGenerationFailed(String),
}
```

Emit with FULL anyhow chain via `format!("{err:#}")` (not just top-level message):

```rust
// In session_ops_title.rs
fn handle_title_result(result: anyhow::Result<Option<String>>) {
    match result {
        Ok(Some(title)) => {
            emit_agent_event(AgentEvent::Session(SessionEvent::TitleUpdated(title)));
        }
        Ok(None) => {}
        Err(err) => {
            warn!("Failed to generate session title: {err}");
            emit_agent_event(AgentEvent::Session(
                SessionEvent::TitleGenerationFailed(format!("{err:#}")),
            ));
        }
    }
}
```

Sweep every `AgentEventSink` consumer:

- **TUI** (`input.rs`): `ErrorText("Title generation failed: {err}")`
- **CLI** (`cli_event_sink.rs`): `eprintln!(warning_text("title generation failed: {err}"))`
- **serve/ag_ui** (`ag_ui.rs`): `emit_custom("session_title_generation_failed", json!({ "error": error }))`
- **Web frontend**: must handle `onStatus` callback for the new event type

**Discipline**: Every `SessionEvent` enum variant must have explicit handling in every sink implementation.

### Fix B — Isolate background-task events with `NullSink`

Wrap spawned generation to suppress intermediate events:

```rust
tokio::spawn(async move {
    let result = harnx_core::sink::with_agent_event_sink(
        Arc::new(harnx_core::event::NullSink),  // Swallow model/streaming/retry events
        Self::generate_title(&config),
    ).await;
    Self::clear_titling(&config, session_id.as_deref());
    handle_title_result(result);  // FINAL result emitted AFTER scope closes
});
```

`with_agent_event_sink` sets a task-local sink override. Events emitted inside `generate_title` go to `NullSink` (dropped). After the scope closes, `handle_title_result` emits the final `TitleUpdated` or `TitleGenerationFailed` to the real global sink.

This is the reusable pattern for "run an LLM sub-call in background without polluting user's UI."

## Why This Works

1. **Dedicated event**: `TitleGenerationFailed` surfaces via existing event infrastructure. Every sink already knows how to render `SessionEvent` variants. Full anyhow chain via `{err:#}` includes root cause (`Miss 'api_key'`).

2. **NullSink isolation**: Task-local sink (`SCOPED_SINK` in `harnx_core::sink`) shadows the global sink for the duration of `with_agent_event_sink`. Title agent's internal completions never reach TUI. After scope exits, global sink restored; final result event reaches user.

3. **Exhaustive match discipline**: Adding a new `SessionEvent` forced inspection of every sink. Compilation fails if match is non-exhaustive.

## Prevention Strategies

**Test Cases:**
- Add E2E test for title generation failure with missing API key
- Verify `TitleGenerationFailed` event content includes root cause
- Verify TUI renders error as `ErrorText`, CLI as warning, web via custom event

**Best Practices:**
- Never rely on `warn!`/`error!` logs for user-visible diagnostics in background tasks
- Mirror the `CompactingFailed` pattern for any new background operation that can fail
- Use `with_agent_event_sink(Arc::new(NullSink), fut)` for background LLM sub-calls
- Emit final result AFTER the NullSink scope closes, not inside

**Code Review Checklist:**
- [ ] New `SessionEvent` variants have handler in every `AgentEventSink`
- [ ] Background tasks emit failure events, not just log messages
- [ ] Error strings include full anyhow chain (`{err:#}`) for root cause visibility
- [ ] Task-local sink override used for background LLM calls that shouldn't pollute UI

**Known Debt:**
`sessions_ops_title.rs` notes that `maybe_compact_session` still lacks this isolation — title-agent events are suppressed but compaction events may still leak. Follow-up candidate.

## Verified Non-Issue

Crossterm `SetTitle` (OSC `ESC]0;...BEL`) reaches terminal under ratatui alternate-screen + raw mode. Written via fresh `std::io::stdout()`. Ratatui redraws do not reset OSC title. No need to route through ratatui backend or re-emit each frame.

## Related Issues

- **GitHub:** [issue #103](https://github.com/dobesv/harnx/issues/103) — Terminal title not updating
- **Related Solution:** [feature-implementation/session-title-generation-pipeline-2026-07-15.md](../feature-implementation/session-title-generation-pipeline-2026-07-15.md) — Original title generation feature implementation
- **Related Solution:** [async-patterns/acp-io-task-supervision-2026-05-07.md](../async-patterns/acp-io-task-supervision-2026-05-07.md) — Supervision for spawned background tasks
