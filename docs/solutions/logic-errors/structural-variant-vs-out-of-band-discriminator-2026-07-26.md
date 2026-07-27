---
title: "Structural enum variant beats out-of-band discriminator for sub-agent provenance"
date: 2026-07-26
category: logic-errors
problem_type: logic_error
component: event-system
root_cause: "out-of-band discriminator parallel to enum invites forgot-to-check bugs"
resolution_type: code_fix
severity: high
tags:
  - enum-design
  - sub-agent
  - event-routing
  - type-safety
  - ACP
  - TUI
plan_ref: subagent-event-variant-1200
---

## Problem

Sub-agent events carried provenance via an out-of-band `source: Option<AgentSource>` parameter on `AgentEventSink::emit`, parallel to the `AgentEvent` enum. Consumers had to remember to check `source.is_some()` before clearing busy state; the TUI missed this, and a nested sub-agent's `ModelEvent::Final`/`Error` flipped the busy→idle spinner while the main loop still ran.

## Symptoms

- TUI showed as not busy while main agent loop was still running (GitHub #1200)
- Nested sub-agent (analyst under researcher) completion events cleared main `llm_busy` flag
- Spinner stopped prematurely during active multi-agent sessions
- Only reproducible with nested sub-agents; single-agent workflows unaffected

```text
Error: TUI `llm_busy = false` set by sub-agent Final/Error
Behavior: Spinner stops, UI appears idle, but agent loop continues
Frequency: Every nested sub-agent completion
```

## Investigation Steps

Started by tracing TUI event handling in `input.rs`. Found `source.is_some()` check routed sub-agent events to heading render but did NOT prevent `llm_busy` clear on `Final`/`Error`. Discovered that every consumer (CLI, TUI, ACP server, AG-UI) had to remember the optional check, and the TUI path missed it.

Reviewed the architecture: `AgentEventSink::emit(event, source: Option<AgentSource>)` meant sub-agent-ness was orthogonal to event type. The TUI's `render_agent_event` matched on `AgentEvent::Model(ModelEvent::Final)` directly without discriminating sub-agent vs main agent.

Designed solution: encode sub-agent provenance INTO the enum as a `SubAgent { source, event }` wrapper variant. This forces every `match` site to handle it or fail to compile.

## Root Cause

Sub-agent-ness was carried out-of-band via an optional parameter parallel to the event enum. This architectural flaw created a "forgot to check" bug class: any match arm processing `Final`/`Error` without checking `source.is_some()` would incorrectly treat sub-agent completion as main completion. The TUI had such a path, and nested sub-agent events (analyst under researcher) triggered it.

The flaw was structural: parameter-based discriminators require every consumer to remember the check, but Rust's exhaustive match already forces consumers to handle every variant. Moving the discriminator into the enum aligns the language's exhaustiveness guarantee with the correctness invariant.

## Solution

### 1. Add structural `AgentEvent::SubAgent` variant

```rust
pub enum AgentEvent {
    // ... existing variants ...
    SubAgent {
        source: AgentSource,
        event: Box<AgentEvent>,
    },
}
```

### 2. Add flatten helper with innermost-source-wins semantics

```rust
impl AgentEvent {
    /// Wraps an event from a sub-agent, preserving the innermost existing source.
    pub fn sub_agent(source: AgentSource, event: AgentEvent) -> AgentEvent {
        let mut source = source;
        let mut event = event;
        while let AgentEvent::SubAgent {
            source: existing,
            event: inner,
        } = event
        {
            source = existing;
            event = *inner;
        }
        AgentEvent::SubAgent {
            source,
            event: Box::new(event),
        }
    }
}
```

The while-loop strips any existing `SubAgent` wrappers and preserves the innermost (first) source. Nested sub-agents (analyst under researcher under parent) must NOT nest `SubAgent`-in-`SubAgent`.

### 3. Remove out-of-band source parameter from trait

```rust
// Before
fn emit(&self, event: AgentEvent, source: Option<AgentSource>);

// After
fn emit(&self, event: AgentEvent);
```

### 4. Add ACP server recursion for SubAgent events

```rust
AgentEvent::SubAgent {
    source: sub_source,
    event,
} => event_to_forward(*event, Some(sub_source)),
_ => None,
```

### 5. TUI structural matching at entry

```rust
let (source, event, is_sub_agent) = match event {
    AgentEvent::SubAgent { source, event } => (Some(source), *event, true),
    event => (None, event, false),
};
```

Sub-agent `Final`/`Error` routes to non-mutating helpers without clearing `llm_busy`.

## Why This Works

**Structural variant forces exhaustiveness:** Rust requires every match to handle all variants. Adding `SubAgent` to the enum means every consumer that matches on `AgentEvent` must either handle it explicitly or use a wildcard. This eliminates the "forgot to check out-of-band param" bug class.

**Innermost-source-wins preserves true origin:** Nested sub-agent chains (analyst → researcher → parent) should preserve the innermost/first source (analyst), not the most recent wrapper (researcher). The helper's while-loop unwraps existing wrappers before re-wrapping, so `sub_agent(researcher, sub_agent(analyst, raw))` yields `SubAgent { source: analyst, event: raw }`.

**ACP recursion forwards embedded source:** The ACP server re-forwards sub-agent events over the wire. The explicit arm `AgentEvent::SubAgent { source, event } => event_to_forward(*event, Some(source))` extracts the embedded source and passes it downstream.

## Mid-Execution Bugs and Lessons

### Bug 1: ACP server dropped SubAgent events

**Location:** `crates/harnx-acp-server/src/lib.rs:event_to_forward`

**What:** The match had a `_ => None` catch-all that silently dropped the new `SubAgent` variant when re-forwarding nested sub-sub-agent events over the wire. Analyst events hit this wildcard and disappeared.

**Fix:** Add explicit arm before the wildcard:
```rust
AgentEvent::SubAgent { source: sub_source, event } => event_to_forward(*event, Some(sub_source)),
_ => None,
```

**Lesson:** When adding an enum variant, audit every wildcard `_ => None`/drop arm in forwarders, serializers, and consumers. Wildcards are maintenance traps because the compiler cannot flag missing handlers.

### Bug 2: Stale binary masked the fix

**What:** The ACP server is a separate binary. A stale `target/debug/harnx-acp-server` from a prior build masked the fix during e2e testing. The tmux test failed even after the code change.

**Fix:** Run `cargo build --workspace` to rebuild all binaries before e2e tests.

**Lesson:** Multi-process architectures (separate server binaries) require full workspace rebuild before end-to-end verification.

### Bug 3: Flatten helper was "most-recent-wins" initially

**What:** First implementation of `AgentEvent::sub_agent` replaced the source on outer wraps instead of preserving innermost. This contradicted the design decision and lost the innermost agent's identity.

**Fix:** Change helper to while-loop that strips existing wrappers and preserves first source.

**Lesson:** Flatten/unwrap semantics must be explicitly documented and tested. Add unit test asserting `sub_agent(researcher, sub_agent(analyst, raw))` yields `source == analyst`.

## Prevention Strategies

### Test Cases

- Unit test: `sub_agent(researcher, sub_agent(analyst, Final))` yields `SubAgent { source: analyst, event: Final }`
- TUI test: sub-agent `Final` does NOT clear `llm_busy`
- TUI test: bare `Final` DOES clear `llm_busy`
- ACP server test: `event_to_forward` with `SubAgent(analyst, chunk)` carries analyst source
- E2E test: nested sub-agent output appears in TUI with correct heading

### Best Practices

1. **Encode discriminators into enums:** When a discriminator (like sub-agent provenance) affects behavior, make it a variant of the enum rather than a parallel parameter. Exhaustiveness checking prevents "forgot to check" bugs.

2. **Audit wildcards on variant additions:** When adding an enum variant, grep for all `_ =>` wildcards in match expressions and verify each one intentionally drops the new variant.

3. **Innermost-source-wins for nested wrappers:** When wrapping events from nested sources, preserve the innermost/first source, not the most recent. Use a while-loop to strip existing wrappers.

4. **Rebuild all binaries before e2e:** Multi-process architectures require `cargo build --workspace` before end-to-end tests.

5. **Use `cargo nextest`, never `cargo test`:** This repo uses `nextest` for process isolation. `cargo test` shares processes and can produce spurious flakes.

### Code Review Checklist

- [ ] Is sub-agent-ness encoded structurally (variant) rather than out-of-band (param)?
- [ ] Does the flatten helper preserve innermost source via while-loop?
- [ ] Are all wildcard `_` arms in forwarders audited for the new variant?
- [ ] Are separate binaries rebuilt before e2e tests?
- [ ] Do TUI sub-agent handlers avoid clearing main `llm_busy`?

## Related Issues

- **GitHub:** [#1200](https://github.com/dobesv/harnx/issues/1200) — TUI shows as not busy but agent loop is still running
- **Related Solution:** [logic-errors/tui-event-source-propagation-2026-05-26.md](./tui-event-source-propagation-2026-05-26.md) — Fragility of optional source tracking (precursor to this structural fix)
