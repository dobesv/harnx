---
title: "Agent handoff architecture: durable detached target activation"
date: 2026-07-19
category: "feature-implementation"
problem_type: logic_error
component: "harnx-runtime, harnx-serve, harnx-tui, harnx-ag-ui"
root_cause: "Recursive source-worker mutation conflated handoff intent with committed target routing"
resolution_type: code_fix
severity: high
tags:
  - agent-handoff
  - session-management
  - nats-worker
  - ag-ui
  - tui
  - durable-activation
plan_ref: "harnx-issues-1091-1540-1515-1467"
last_updated: 2026-08-26
---

## Problem

Handoffs used to switch the source worker's `Config`, session backend, hook
supervisor, and lease, then recursively continue the target turn in the same
task. That model violated the identity and lifecycle of both sessions:

- omitting `session_id` could substitute the source session ID;
- the source lease and persistence sink were repurposed for the target;
- the target was not activated through its ordinary actor/worker path;
- Web and TUI navigated on a request before a usable destination existed;
- queued serve actor prompts could be lost to an idle-reap race; and
- sub-agent activity had no durable, navigable TUI view.

The critical distinction is between **handoff intent** and **handoff
commitment**. A model requesting a transfer does not prove that target metadata
was created, the prompt was persisted, or activation was published.

## Current Contract

`LoopResult::HandoffRequested { agent, session_id, prompt }` remains the engine's
internal intent. Blank and whitespace IDs are normalized to `None`; the source
ID is never substituted.

The session ID contract exactly matches `session_prompt`:

1. Omitted or blank ID creates a generated target-agent session.
2. An unused explicit ID creates that exact target-agent session.
3. An existing target-owned ID continues its durable transcript.
4. An ID owned by another agent fails before appending the prompt.

Two events expose the lifecycle:

```text
Turn::HandoffRequested { agent, session_id? }
    informational; emitted on the source stream before dispatch

Session::HandoffCommitted { agent, session_id }
    control event; emitted only after durable enqueue + activation
```

AG-UI maps those to `turn_handoff_requested` and `session_handoff`,
respectively. `session_handoff` always carries nonempty strings and remains a
live, non-replayed navigation signal.

## NATS Execution

The source worker no longer becomes the target. On a handoff it:

1. Resolves the target agent and cluster.
2. Opens or creates the canonical target `NatsSession`.
3. Appends the user prompt to the target log.
4. Publishes activation using the worker's existing route: frontend-targeted
   for a local worker, cluster-shared for a persistent worker. Explicit
   `agent@cluster` references use that configured cluster and a shared route.
5. Emits `HandoffCommitted` on the source stream and crosses an ordered,
   fallible event-sink flush barrier.
6. Records the source `TurnEnd` and stops draining that source activation.

The target's normal activation performs metadata loading, ownership validation,
lease acquisition, hook reconciliation, `SessionStart`, queued-prompt draining,
and completion recording. Existing target history is preserved. The source's
configuration, session, hook supervisor, persistence backend, and lease remain
unchanged.

The enqueue helper deliberately shares append and activation code with
`NatsSession::run_turn`. Prompt persistence therefore precedes activation in
both paths. If target creation or activation fails, the source turn fails and
no committed event is emitted. If confirmation delivery fails after activation,
the source error includes the resolved target session ID for manual recovery;
there is no unsafe rollback of an already queued target turn.

The worker drain loop treats a dispatched handoff as a terminal source outcome.
Without this explicit outcome, reconstructing the completed handoff tool result
as a resumable round can dispatch the same target prompt repeatedly.

## Serve Actor Execution

The in-process test executor implements the same identity rules. It sends the
prompt to the target `SessionActor`, awaits the prompt acknowledgement, emits
`HandoffCommitted`, and only then emits source `RUN_FINISHED`. Send or
acknowledgement failure becomes source `RUN_ERROR`.

A same-agent/same-session handoff cannot send to and await its own mailbox. It
is appended directly to that actor's pending queue and replayed after the source
run boundary.

Idle reaping requires all of the following:

- the deadline has elapsed;
- there are no subscribers;
- no run is active;
- the command mailbox is empty; and
- no external sender handle is held.

`SessionRegistry::has_session` also rejects entries whose actor channel is
already closed.

## Web and TUI Behavior

Web retains its active-run gate but ignores `turn_handoff_requested`. It
navigates only on a `session_handoff` event containing nonblank `agent` and
`session_id`. Mounting the target route attaches its normal passive subscriber,
which hydrates durable target history and follows live updates.

TUI also treats the request as informational. On commitment it opens the
resolved agent/session, reloads target history, resets root browsing/streaming
state, and attaches the normal session-activity monitor. The old prompt task's
completion carries its old abort token and cannot clear the newly selected
target state.

Nested delegation remains distinct from handoff. `SubAgentStarted` creates a
compact selectable row and starts an independent child `SessionEventStream`
monitor. Child history, live transcript, status, focus, scrolling, and task
handle are isolated from root state. Focusing the row opens the child in the
fullscreen transcript surface; nested rows drill into grandchildren and `Esc`
returns one level.

The durable `sub_agent` marker updates the latest active row or creates a
completed fallback row when the early advisory was missed. It replaces the
generic tool-result body so the child response is not duplicated. Child
monitors are aborted and their retained views discarded when the root session
changes or the TUI exits.

## Review Checklist

- [ ] Are omitted, empty, and whitespace handoff IDs normalized to `None`?
- [ ] Does explicit-ID ownership fail before any target prompt append?
- [ ] Is the target prompt durable before activation is published?
- [ ] Does the source retain its config, backend, hooks, session, and lease?
- [ ] Does `HandoffCommitted` precede the source terminal event and carry a
      nonblank resolved ID?
- [ ] Does a dispatched handoff stop source resumable-round draining?
- [ ] Do Web and TUI ignore raw requests for navigation?
- [ ] Can same-session actor handoff complete without awaiting itself?
- [ ] Does idle reaping require an empty mailbox and a live registry channel?
- [ ] Are child-session events isolated from root TUI busy/input/streaming state?

## Related Issues and Notes

- [#1091](https://github.com/dobesv/harnx/issues/1091)
- [#1540](https://github.com/dobesv/harnx/issues/1540)
- [#1515](https://github.com/dobesv/harnx/issues/1515)
- [#1467](https://github.com/dobesv/harnx/issues/1467)
- [NATS nested sub-agent toolset](./nats-nested-subagent-toolset-2026-07-31.md)
- [Session actor concurrency invariants](../async-patterns/session-actor-concurrency-invariants-2026-07-04.md)
- [Package-relative agent handoff](../logic-errors/package-relative-agent-handoff-2026-06-04.md)
