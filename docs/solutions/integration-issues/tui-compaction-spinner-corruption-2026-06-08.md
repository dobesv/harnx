---
title: "TUI compaction transcript via AgentEvent instead of stdout spinner"
date: 2026-06-08
category: integration-issues
problem_type: integration_issue
component: "harnx-runtime, harnx-tui, harnx-core"
root_cause: "Direct stdout writes (crossterm spinner) bypassed ratatui's terminal state management, corrupting input area and leaving uncleared line"
resolution_type: code_fix
severity: medium
tags:
  - tui
  - ratatui
  - crossterm
  - agent-event-sink
  - transcript
  - compaction
plan_ref: compaction-transcript-711
---

## Problem

Compaction in the TUI (`Cmd+. compact session`) drove a stdout spinner using `harnx-spinner` and crossterm escape sequences. These direct stdout writes corrupted the ratatui-managed input area and left an uncleared line after compaction finished.

## Symptoms

- Spinner text ("Compacting...") appeared in the TUI input area, overwriting the prompt
- After compaction completed, a stray line remained visible in the terminal
- TUI display state became inconsistent until next redraw

## Investigation Steps

1. Traced the `.compact session` path in `harnx-runtime/src/commands.rs` — used `abortable_run_with_spinner()` which writes directly to stdout via `crossterm`
2. Confirmed harnx has a process-level `AgentEvent` sink system (`harnx_core::sink`) used for TUI/CLI/ACP frontends
3. Found `harnx_core::sink::has_agent_event_sink()` predicate that returns `true` when a sink is installed
4. Discovered `SessionEvent` variants are rendered in `harnx-tui/src/input.rs` via `render_agent_event()`
5. Noted the catch-all `_ => vec![]` silently drops unhandled event variants — missing explicit arms

## Root Cause

Two issues combined:

1. **Direct stdout writes corrupt ratatui**: `abortable_run_with_spinner()` uses `crossterm` to write spinner frames directly to stdout. Ratatui maintains its own terminal state via alternate screen buffer; direct writes bypass this and corrupt the display.

2. **No event-based compaction path**: TUI mode had no way to render compaction progress as structured transcript entries instead of raw stdout.

## Solution

### 1. Emit lifecycle events unconditionally from the runtime

In `crates/harnx-runtime/src/commands.rs`, the `.compact session` command always
emits `SessionEvent`s — it does **not** branch on `has_agent_event_sink()`. (An
earlier draft gated on that predicate, but it was removed: plain CLI also installs
a sink, so the predicate is `true` in both CLI and TUI, making the gate dead code
and routing CLI output into the sink's debug catch-all.) The command also claims
the session's `compressing` flag atomically under a single write lock so manual
compaction is visible to auto-compaction and the agent loop:

```rust
".compact" => match args {
    Some("session") => {
        // Atomic check-and-claim of the `compressing` flag under one write lock.
        // (Distinguish NoSession / AlreadyCompacting / Claimed.)
        // ... if not Claimed, print a message and return Ok(Continue) ...

        harnx_core::sink::emit_agent_event(AgentEvent::Session(
            SessionEvent::CompactingStarted,
        ));
        let result = Config::compact_session(config).await;
        // Always clear the flag afterwards.
        if let Some(session) = config.write().session.as_mut() {
            session.set_compressing(false);
        }
        match result {
            Ok(()) => emit(SessionEvent::CompactingCompleted),
            // Emit failure event ONLY — do not also writeln! or propagate,
            // or the failure renders twice.
            Err(err) => emit(SessionEvent::CompactingFailed(err.to_string())),
        }
    }
    _ => writeln!(output, r#"Usage: .compact session"#)?,
},
```

Auto-compaction (`maybe_compact_session` in `config/session_ops_split.rs`) emits
the same `CompactingStarted` → `CompactingCompleted`/`CompactingFailed` sequence.

### 2. Add `SessionEvent` variants

In `crates/harnx-core/src/event.rs`:

```rust
pub enum SessionEvent {
    // ... existing variants
    CompactingStarted,
    CompactingCompleted,
    CompactingFailed(String),
}
```

### 3. Render events in each sink

The runtime emits events unconditionally; each frontend's sink decides how to
render them.

TUI — `crates/harnx-tui/src/input.rs`, explicit arms in `render_agent_event()`:

```rust
AgentEvent::Session(SessionEvent::CompactingStarted) => {
    vec![TranscriptItem::SystemText("Compacting session…".to_string())]
}
AgentEvent::Session(SessionEvent::CompactingCompleted) => {
    vec![TranscriptItem::SystemText("Session compacted.".to_string())]
}
AgentEvent::Session(SessionEvent::CompactingFailed(err)) => {
    vec![TranscriptItem::ErrorText(format!("Compaction failed: {err}"))]
}
```

CLI — `crates/harnx/src/cli_event_sink.rs` adds arms that drive a managed
spinner (start on `CompactingStarted`, `cleanup()` + "✓ Compacted the session."
on `CompactingCompleted`, `cleanup()` + warning on `CompactingFailed`). This
replaces the previous `[event] …` debug-catch-all fallback for these variants.

### 4. Transcript reconciliation clears transient entries

`reconcile_transcript_after_command()` clears and rebuilds the transcript for `.compact session`. The transient "Compacting session…" entry provides in-flight feedback; the final state comes from the rebuilt transcript (e.g., "─── session compacted ───" divider).

## Why This Works

**Process-level sink registry**: `harnx_core::sink` provides a global `AgentEventSink` registry. Frontends install their sink at startup, enabling any code (commands, runtime) to emit events without coupling to specific UIs.

**Unconditional emission, sink-side rendering**: the runtime always emits the
`SessionEvent`s; it does not gate on `has_agent_event_sink()`. Each installed
sink renders them appropriately (TUI → transcript items, CLI → managed spinner +
messages). `has_agent_event_sink()` is a poor branch point here because every
frontend — including the plain CLI — installs a sink, so the predicate is almost
always `true`; gating on it left the "no sink" branch dead and pushed CLI output
into the sink's debug catch-all.

**Ratatui owns terminal state**: Events rendered via `render_agent_event()` go through ratatui's widget pipeline. No raw stdout writes means no corruption.

**Explicit match arms prevent silent drops**: Adding explicit arms for new `SessionEvent` variants ensures they're rendered. The catch-all `_ => vec![]` silently ignores unknown events — better to enumerate all handled variants.

## Prevention Strategies

**Test Cases:**
- Add render tests for `CompactingStarted`/`CompactingCompleted`/`CompactingFailed` events (verify the TUI transcript contains the expected text / error item)
- Verify the CLI event sink renders compaction events as a managed spinner (start) and clears it on completion/failure — not the `[event] …` debug fallback

**Best Practices:**
- Emit structured `SessionEvent`/`AgentEvent` lifecycle events; let sinks handle rendering for CLI vs TUI. Do not gate emission on `has_agent_event_sink()` — every frontend installs a sink.
- Add explicit match arms in `render_agent_event()` (and the CLI sink's `emit`) for new `SessionEvent`/`AgentEvent` variants so the catch-all never silently drops or debug-prints them
- Never write directly to stdout/stderr from code paths reachable in TUI mode — use the event sink

**Code Review Checklist:**
- [ ] Does this code write to stdout/stderr directly from a TUI-reachable path? Route it through the event sink instead.
- [ ] Are new `AgentEvent`/`SessionEvent` variants handled by BOTH the TUI `render_agent_event()` and the CLI sink's `emit` (not just the catch-all)?
- [ ] Does `reconcile_transcript_after_command()` need updating for new mutation commands?

## Related Issues

- **Issue:** [#711](https://github.com/dobesv/harnx/issues/711) — Compaction spinner corrupting TUI
- **Prior Solution:** [tui-event-source-propagation-2026-05-26.md](./tui-event-source-propagation-2026-05-26.md) — Event source propagation pattern
- **Prior Solution:** [non-interactive-safe-defaults-bash-children-2026-04-30.md](./non-interactive-safe-defaults-bash-children-2026-04-30.md) — Similar theme: preventing TUI corruption from child processes
