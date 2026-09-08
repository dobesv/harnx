---
title: "AG-UI interrupt resume TOCTOU race (follow-up)"
date: 2026-09-08
last_verified: 2026-09-08
component: "crates/harnx-serve"
problem_type: integration_issue
status: follow-up
anchors:
  - crates/harnx-serve/src/session_actor.rs:handle_prompt
  - crates/harnx-serve/src/interrupt_resume.rs:validate_resume
tags:
  - ag-ui
  - interrupt
  - race-condition
  - tool-confirmation
plan_ref: "web-ui-tui-parity-1741-1742"
---

# AG-UI Interrupt Resume TOCTOU Race

## When this is relevant

Two concurrent resume submissions for the same interrupted tool-approval batch. The second request can race with the first, get queued as an ordinary `PendingPrompt`, and later replay via `start_run` applying stale `options.resume` tool-confirmation overrides to a fresh unrelated turn.

Symptoms:
- Concurrent `session/prompt` calls with `resume` field for same batch
- Second client receives `Enqueued { run_id }` instead of a validation error
- Later turn executes with tool-confirmation settings from a prior interrupted turn

## Durable lesson

The session actor's `handle_prompt` queues prompts when a run is active (lines 382-395). Resume submissions have a TOCTOU window: the first resume clears the `Interrupted` state, and a concurrent second resume can race into the pending queue. `validate_resume` only checks the current `SessionState` at parse time, not at `start_run` replay time.

The queue/replay mechanism is pre-existing (used for NATS/TUI mid-turn injection), but the AG-UI SSE path newly exposes this web endpoint to concurrent resume submissions.

## Evidence and current anchors

- `session_actor.rs:382-395` — `handle_prompt` queues `PendingPrompt { text, options }` when `active_run` exists
- `session_actor.rs:493-497` — replay uses `pending.text` and `pending.options` verbatim
- `interrupt_resume.rs:108-167` — `validate_resume` checks `SessionState::Interrupted` but not queue context
- Plan note `a7ae8c37` — TOCTOU race identified and downgraded to follow-up by review majority

## Suggested fix

Either:
1. Re-validate resume applicability at `start_run` time: check that `options.resume` still matches a live `Interrupted` state (fails fast if state changed)
2. Strip `resume` from queued prompts before replay: `PendingPrompt.options.resume = []` so replayed prompts run as fresh turns

Option 2 is simpler and avoids surprising retry semantics. Option 1 preserves intentional resume-across-restart scenarios if those become desirable.

## Failed approaches or trade-offs

Guarding at the HTTP layer (per-request mutex) doesn't help: the queue is inside the actor's serialized command loop, and the race is between `handle_prompt` queue insertion and `handle_run_done` state transition.

Deferring to follow-up was accepted because:
- Requires concurrent same-batch resume submissions (low-probability UI interaction)
- Requires tool-call-id reuse across turns (low probability unless client generates IDs)
- Pre-existing queue mechanism, newly exposed via SSE endpoints
