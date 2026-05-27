---
title: "TUI event source propagation and model-change heading updates"
date: 2026-05-26
category: logic-errors
problem_type: logic_error
component: tui-event-system
root_cause: "sourceless event emissions and missing source state seeding"
resolution_type: code_fix
severity: high
tags:
  - tui
  - events
  - streaming
  - fallback
  - heading-rendering
plan_ref: issue-274-model-in-tui
---

## Problem

The TUI's streaming path did not emit sourced `TurnEvent::Started`, leaving `last_ui_output_source` as `None` at session start. This caused model-change event handlers (for `ModelFallback` and `ModelChanged`) to silently no-op because they map over the optional source. Result: dynamic heading insertion failed on model fallback or `.model` command.

## Symptoms

- TUI headings did not update when model changed via `.model <name>` command
- Model fallback during retry loop did not insert new heading in transcript
- `last_ui_output_source` remained `None` until first `MessageChunk` event
- Event handlers for `ModelFallback` and `ModelChanged` executed but produced no visible effect

## Investigation Steps

1. Traced event flow from emission to TUI handler:
   - `harnx-engine/src/retry.rs` emits `TurnEvent::ModelFallback { from, to }` via `ctx.emit()`
   - `harnx-runtime/src/commands.rs` emits `SessionEvent::ModelChanged { from, to }` via `emit_agent_event()`
   - Both events reach TUI via global sink (`harnx-core/src/sink.rs`)

2. Found TUI handlers in `input.rs:919-933`:
   ```rust
   if let AgentEvent::Turn(TurnEvent::ModelFallback { ref to, .. }) = event {
       let new_source = self.app.last_ui_output_source.clone().map(|mut s| {
           s.model = Some(to.clone());
           s
       });
       self.render_ui_output_heading(new_source.as_ref(), false);
       return;
   }
   ```

3. Discovered `last_ui_output_source` starts as `None` in TUI sessions:
   - CLI path (`common.rs`) emits sourced `TurnEvent::Started`
   - TUI path (`prompt.rs`) did NOT emit sourced `Started` before retry loop
   - Result: no source to clone when fallback event arrives

4. Verified `MessageChunk` events are sourceless:
   - `emit_agent_event()` used for streaming chunks (no source parameter)
   - Source only available via `last_ui_output_source` cache

## Root Cause

Two related issues:

1. **Missing sourced Started emission**: TUI's `prompt.rs` called `call_with_retry_and_fallback_custom` directly without first emitting `TurnEvent::Started` with an `AgentSource`. The CLI path in `common.rs` does emit this event, establishing `last_ui_output_source` early.

2. **Sourceless streaming events**: `MessageChunk` and `ThoughtChunk` events use `emit_agent_event()` (sourceless) rather than `emit_agent_event_with_source()`. The TUI relies on `last_ui_output_source` as a cache set by prior events.

Without seeded source state, model-change handlers clone `None` → map does nothing → `render_ui_output_heading(None, false)` is a no-op.

## Solution

### 1. Emit sourced `TurnEvent::Started` in TUI path

In `crates/harnx-tui/src/prompt.rs`, before calling the retry loop:

```rust
// Seed the UI source so ModelFallback can build a new heading
{
    use harnx_core::event::{AgentEvent, AgentSource, TurnEvent};
    let agent_source = {
        let cfg = ctx.config.read();
        let agent_ref = cfg.extract_agent();
        let agent = agent_ref.name().to_string();
        let session_id = cfg.session.as_ref().map(|s| s.id().to_string());
        let model = cfg.session.as_ref().map(|s| s.model().id().to_string())
            .or_else(|| Some(agent_ref.model().id().to_string()))
            .or_else(|| Some(cfg.model.id().to_string()));
        AgentSource { agent, session_id, model }
    };
    harnx_core::sink::emit_agent_event_with_source(
        AgentEvent::Turn(TurnEvent::Started),
        Some(agent_source),
    );
}
```

### 2. Wire `event_fn` in TurnContext

In `crates/harnx-runtime/src/client/retry.rs`:

```rust
event_fn: Arc::new(|event: harnx_core::event::AgentEvent| {
    harnx_core::sink::emit_agent_event(event);
}),
```

### 3. Emit ModelFallback from retry loop

In `crates/harnx-engine/src/retry.rs`:

```rust
// Track previous model to detect transitions
let mut prev_tried_model: Option<String> = None;

for (idx, model_id) in model_ids.iter().enumerate() {
    // Skip cooldown check...
    
    // Emit ModelFallback when switching models after failure
    if let Some(ref prev) = prev_tried_model {
        if prev != model_id {
            ctx.emit(AgentEvent::Turn(TurnEvent::ModelFallback {
                from: prev.clone(),
                to: model_id.clone(),
            }));
        }
    }
    
    // Attempt call...
    prev_tried_model = Some(model_id.clone());
}
```

### 4. Emit ModelChanged from .model command

In `crates/harnx-runtime/src/commands.rs`:

```rust
let from_model = config.read().current_model().id().to_string();
config.write().set_model(name)?;
let to_model = config.read().current_model().id().to_string();
harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Session(
    harnx_core::event::SessionEvent::ModelChanged {
        from: from_model,
        to: to_model,
    },
));
```

## Why This Works

1. **Sourced Started seeds state**: Emitting `TurnEvent::Started` with an `AgentSource` before the retry loop sets `last_ui_output_source` in the TUI. This provides the base source that model-change handlers clone and modify.

2. **Early return prevents overwrite**: The `ModelFallback` handler calls `render_ui_output_heading()` then `return`. Subsequent events with stale source cannot overwrite because the handler already exited.

3. **Event comparison guards against duplicates**: `render_ui_output_heading()` compares new source against `last_ui_output_source`. If identical, no heading is inserted. This prevents duplicate headings from redundant events.

4. **Global sink wiring completes the path**: `TurnContext.emit()` routes through `event_fn` → global sink → TUI event loop → handler.

## Prevention Strategies

### Test Cases

- Unit test: verify `last_ui_output_source` is `Some` after `TurnEvent::Started` emission
- Integration test: model fallback inserts new heading with fallback model name
- Integration test: `.model <name>` command inserts new heading with new model name
- E2E test: `retry_succeed_after_fallback_shows_transition` validates end-to-end behavior

### Code Review Checklist

- [ ] Do event handlers that depend on cached state have that state seeded before use?
- [ ] Are events with source emitted using `emit_agent_event_with_source()`?
- [ ] Do TUI and CLI paths have parity for source seeding?
- [ ] Are model-change events wired from emission to handler?

### Patterns to Follow

1. **Seed before use**: If an event handler depends on cached state, ensure that state is seeded at a known point before the handler can be invoked.

2. **Early return for emergency updates**: Handlers that render "emergency" headings (like model fallback) should early-return to prevent later events from overwriting.

3. **Source comparison guards**: Heading rendering should check if the new source differs from cached source before inserting.

## Related Issues

- **Jira:** [harnx#274](https://github.com/dobesv/harnx/issues/274) — Display model in TUI headings and status bar
- **Related Solution:** [logic-errors/non-tui-terminal-output-fixes-2026-04-30.md](./non-tui-terminal-output-fixes-2026-04-30.md) — `last_ui_output_source` tracking for heading deduplication
