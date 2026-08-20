---
title: "Session actor concurrency invariants for mid-loop prompt injection and reaping"
date: 2026-07-04
category: async-patterns
problem_type: logic_error
component: harnx-serve
root_cause: "subtle race conditions in actor state transitions across run completion, reap timing, and mid-loop injection boundaries"
resolution_type: code_fix
severity: high
tags:
  - tokio
  - actor
  - select
  - mpsc
  - broadcast
  - concurrency
  - cancellation
  - session-actor
  - mid-loop-injection
  - reap-race
  - dashmap
plan_ref: ag-ui-serve-control-plane
last_updated: 2026-08-20
---

## Problem

Per-session actor in `harnx-serve` exhibited subtle race conditions:
1. **Prompt loss at completion boundary**: Prompts enqueued while a run was completing could be silently dropped.
2. **Reap races**: Actors could be reaped right after a new subscriber joined, or while a caller was holding their handle.
3. **Mid-loop injection data loss**: Multi-prompt injection drained all queued prompts but kept only one, silently discarding the rest.
4. **Test hangs**: SSE subscription streams drained to completion in tests blocked forever; `Notify::notify_waiters()` lost signals on multi-threaded runtimes.

## Symptoms

- **Prompt loss**: `session/prompt` during run completion never triggered a subsequent run.
- **Reap race (subscribe)**: After `Unsubscribe` + quick `Subscribe`, the actor sometimes disappeared from registry.
- **Reap race (caller)**: `session/get`, `session/prompt`, and `session/cancel` intermittently returned HTTP 503 with `-32003` and the message `session actor unavailable` (or `session actor dropped … reply`) for a session that existed — ~1 flake in 10 runs of `rpc_session_prompt_returns_ack_and_persists_effect`.
- **Data loss**: Enqueuing 2+ prompts during a run injected only the first; others silently dropped.
- **Test hangs**: E2E tests with `body.collect().await` on SSE streams hung indefinitely; tests using `Notify::notify_waiters()` deadlocked on multi-threaded runtimes.
- **Config panic**: Test helper `load_base_config_for_tests()` (which calls `std::env::set_current_dir` and expects `HARNX_CONFIG_DIR`) used in production paths caused thread-safety violations and panics.

## Root Cause

### 1. Missing COMPLETING-STATE Invariant

The actor had no explicit "completing" state to buffer prompts arriving during run finish. The `inject_tx.try_send` could return `Err(Full|Closed)` during the run-done transition, and without a `pending` queue, prompts were lost.

### 2. Reap Races

Two distinct races, both ending in a registry entry removed too early.

**Subscribe vs. timer (2026-07):** on `Unsubscribe` when `subscribers == 0 && !running`, the actor armed a reap timer. A concurrent `Subscribe` canceled it, but the timer arm didn't re-check the subscriber count before removing from `DashMap`. A just-revived actor could be incorrectly reaped.

**Caller vs. reap (2026-08, issue #1465):** re-checking the actor's own state fixed the first race but not this one, because `subscribers`/`is_running` say nothing about callers holding a handle. `get_or_spawn` could clone a still-registered handle *after* the actor decided to reap and *before* its `registry.remove` landed; the caller's `send` then hit a closed channel and the RPC returned 503 (`-32003`, message `session actor unavailable` or `session actor dropped … reply`). A `tx.is_closed()` check in `get_or_spawn` narrows this window but cannot close it: the map stores a live `Sender`, so `is_closed()` only flips once the actor has already dropped `rx`. Separately, an unconditional `registry.remove(&self.key)` on exit could evict a *replacement* actor's entry if the outgoing actor exited late.

### 3. Drain-All-Keep-First Anti-Pattern

The `on_tool_round` hook drained the inject channel with `while let Ok(text) = inject_rx.try_recv()` but only kept the first message, discarding others. Multiple prompts during a run silently lost data.

### 4. Test Hazard: Long-Lived SSE Drain

Tests calling `body.collect().await` on SSE streams assumed the stream would end. The new subscription model keeps streams open indefinitely; draining to completion hangs forever.

### 5. Test Hazard: Notify::notify_waiters()

`tokio::sync::Notify::notify_waiters()` drops the signal if no waiter is registered yet. On multi-threaded runtimes, this caused lost wakeups and deadlocks.

### 6. Config Threading Violation

`load_base_config_for_tests()` used in actor production paths:
- Called `std::env::set_current_dir` (process-wide, thread-unsafe in async server)
- Expected `HARNX_CONFIG_DIR` env var (panics if missing in production)

## Solution

### 1. COMPLETING-STATE Invariant (no separate state needed)

Keep the actor `Running` until the run-done arm fires. The done arm **atomically** (no `.await` mid-iteration) drains a `pending: VecDeque<String>` and either spawns the next run or goes `Idle`.

```rust
// In run-done arm of select! loop
fn handle_run_done(&mut self, result: RunResult) {
    self.running = None;
    self.abort_signal = None;
    
    // ATOMIC: drain pending without await
    if let Some(next_prompt) = self.pending.pop_front() {
        self.start_run(next_prompt);  // Stay Running
    } else {
        // Go Idle; reap timer arms if subs==0
    }
}
```

**On Prompt while Running**: If `inject_tx.try_send()` returns `Err(Full|Closed)`, push to `pending` queue. No lost prompts across finish boundary.

### 2. Reap Guard: State Re-check *Plus* an Identity/Refcount Gate

Layer one (unchanged since 2026-07): on last unsubscribe (`subs == 0 && !running`) arm a reap `Sleep` for `DEFAULT_REAP_TTL` (5s); on `Subscribe` cancel it; in the timer arm re-check `self.subscribers == 0 && !self.is_running()` before doing anything destructive (`SessionActor::run` reap branch and `should_reap()`).

Layer two (2026-08): make the removal itself the decision point, so no caller can observe the actor mid-reap. Both conditions are evaluated inside the same `DashMap` operation that removes the entry — `SessionActor::deregister_for_reap` in `crates/harnx-serve/src/session_actor.rs`:

```rust
self.registry.remove_if(&self.key, |_, handle| {
    handle.actor_id == self.actor_id && handle.tx.strong_count() == 1
})
```

- `SessionHandle::actor_id` (monotonic `AtomicU64`, assigned in `spawn_session_actor`) gives each map entry an identity. Every self-removal path is gated on it (`deregister()` for the two channel-closed exits, `deregister_for_reap()` for the idle reap), so an exiting actor can never evict its replacement. There is no bare `registry.remove()` in the actor anymore.
- `tx.strong_count() == 1` means the registry's stored handle holds the only `Sender`, i.e. no caller is mid-request. `spawn_session_actor` drops its local `tx` and the actor keeps only `rx`, so 1 really is the idle count. If the predicate rejects, the actor re-arms its deadline and tries again later.

**Invariant this creates: holding a `SessionHandle` clone pins its actor against reaping.** A clone stored anywhere long-lived leaks an actor for as long as it lives, with no log line or metric. The only long-lived holder today is the SSE path, which releases via `UnsubscribeOnDrop` in `crates/harnx-serve/src/ag_ui.rs`.

The atomicity rests on a dashmap implementation detail, not a documented API guarantee: in dashmap 6.2.1 both `_remove_if` and `_entry` take the write lock of the key's shard (`_yield_write_shard`), and `_remove_if` runs the predicate *and* the removal while holding it, so `get_or_spawn`'s `entry()` can't hand out a clone in between. Re-verify on a dashmap upgrade — no test covers it.

Read side, defense in depth: `get_or_spawn_in` in `crates/harnx-serve/src/session_actor/registry.rs` treats an entry whose `tx` is closed as vacant and spawns a replacement (logging a warning), so an actor task that died without deregistering (a panic, say) no longer 503s that key forever.

Asymmetry to know about before "fixing" it: `SessionRegistry::has_session` reports a closed entry as present while `get_or_spawn` treats the same entry as vacant. That's what keeps the 404 gate in `ag_ui_rpc.rs` (`!session_exists(..) && !registry.has_session(..)`) open long enough for the replacement to spawn. Making `has_session` liveness-aware would turn a crashed in-memory-only session into a 404 instead of a self-heal.

Tests: `crates/harnx-serve/src/session_actor/tests/session_actor_registry_tests.rs`.

### 3. One-Per-Round Injection

Consume **ONE** prompt per tool round, not drain-all-keep-first:

```rust
// in on_tool_round hook
if let Ok(text) = inject_rx.try_recv() {
    merged_input.set_injected_user_text(text);
    // Remaining prompts stay queued for next round
}
```

This matches the loop's one-shot `set_injected_user_text` behavior and preserves FIFO across rounds.

### 4. Cancellation via AbortSignal

**NEVER** drop the run future to cancel. Use `AbortSignal::set_ctrlc()`:

```rust
// On session/cancel
if let Some(ref signal) = self.abort_signal {
    signal.set_ctrlc();  // Loop unwinds and persists partial state
}
```

Dropping skips persistence; `set_ctrlc()` lets the loop unwind gracefully (D5).

### 5. Test Hazard: Bounded SSE Reads

Never drain SSE streams to end in tests. Read bounded:

```rust
// Until RUN_FINISHED or RUN_ERROR (terminal events)
while let Some(event) = read_sse_event_with_timeout(&mut body, 5s).await {
    if matches!(event, RunFinished | RunError) { break; }
}

// Or take-N with timeout
let events = read_n_events_with_timeout(&mut body, 10, 5s).await;
// Then DROP the stream
drop(body);
```

Seed persisted sessions via `registry.get_or_spawn + prompt + sleep`, NOT by collecting SSE streams.

### 6. Test Hazard: notify_one() Over notify_waiters()

Use `notify_one()` (stores a permit) instead of `notify_waiters()`:

```rust
// WRONG: drops signal if no waiter yet
ready_notify.notify_waiters();

// RIGHT: stores permit for next waiter
ready_notify.notify_one();
```

Or ensure waiter is registered before trigger, or gate via channels/`AtomicBool`.

### 7. Config Threading

Thread the server's real `Config` through `SessionRegistry::new(config)` (now in `crates/harnx-serve/src/session_actor/registry.rs`, which stores it inside `SessionActorConfig`). Each actor forks a per-run copy and scopes it to its own agent/session via `prompt_config_for_agent_session_from_global` (`crates/harnx-serve/src/session_actor.rs`) instead of reloading from env.

**Test isolation**: `TestConfigSandbox` (`crates/harnx-serve/src/test_support.rs`) mutates process env and briefly `set_current_dir`s while loading config, so it holds a process-global mutex for its whole lifetime. That plus nextest's per-test process isolation is what keeps these tests honest — run them with `cargo nextest`, never `cargo test`, per the root `AGENTS.md`.

## Why This Works

- **Atomic drain**: No `.await` between run-done and pending check ensures no race window for prompt injection at completion boundary.
- **Re-check before reap**: Eliminates the race between subscribe and timer arm; a just-revived actor survives. It says nothing about callers, which is why the removal predicate carries the rest.
- **Identity + refcount gate on removal**: The reap decision and the removal happen under one shard lock, so a caller either gets a handle to a live actor or spawns a fresh one — never a handle to an actor on its way out.
- **One-per-round**: Preserves all queued prompts across rounds; no silent drops.
- **AbortSignal cancellation**: Preserves persistence semantics; partial state committed before unwind.
- **Bounded test reads**: Tests terminate; streams don't hang forever.
- **notify_one()**: Guarantees the next waiter wakes even if signal sent before registration.
- **Config threading**: Production paths never touch test-only env/chdir helpers; thread-safe config access.

## Prevention Strategies

### Test Cases

- **Prompt at finish boundary**: Enqueue prompt while run completing; assert subsequent run spawns with correct prompt.
- **Reap race**: Unsubscribe, sleep briefly, re-subscribe before TTL; assert actor survives and broadcast works.
- **Reap with a handle held**: Arm the deadline, sleep well past the TTL while keeping the handle, then send a command; assert it still answers and the same `actor_id` is registered. Drop the handle and assert the actor *is* reaped — otherwise a fix that just stops reaping passes.
- **Replacement not evicted**: Register two incarnations under one key; assert the outgoing actor's exit leaves the replacement's entry in place.
- **Dead entry self-heals**: Insert a handle whose receiver is dropped; assert `get_or_spawn` returns a working actor with a different `actor_id`.
- **Multi-prompt injection**: Enqueue 2+ prompts during run; assert BOTH land across successive tool rounds.
- **Cancel persistence**: Cancel mid-run; assert partial state persisted (persistence is NOT skipped).
- **SSE bounded read**: Test helper `read_sse_events_until(terminal_pred, timeout)` that returns partial, never hangs.
- **Notify signal**: Test with `#[tokio::test(flavor = "multi_thread")]`; verify `notify_one()` wakes waiter registered after signal.

### Code Review Checklist

- [ ] Does actor `select!` avoid `.await` between state transitions?
- [ ] Are timers/callbacks re-checking invariants before destructive actions?
- [ ] Does every actor self-removal go through an `actor_id`-gated `remove_if`, never a bare `registry.remove(&self.key)`?
- [ ] Is each new `SessionHandle` clone short-lived or paired with an explicit drop path? A stored clone pins its actor against reaping for as long as it lives.
- [ ] Is injection one-per-round (not drain-all-keep-first)?
- [ ] Does cancellation use `AbortSignal::set_ctrlc()`, not future drop?
- [ ] Do test SSE streams read bounded with timeouts, not `collect().await`?
- [ ] Is `notify_one()` used instead of `notify_waiters()` for single-waiter signaling?
- [ ] Are test-only helpers excluded from production paths (grep for `load_base_config_for_tests`, `set_current_dir`, env `.expect`)?
- [ ] Does `SessionRegistry`/actor hold the server's real `Config`, not reload from env?
- [ ] Are actor-owned child tasks held in `AbortOnDropHandle`, not bare `JoinHandle`? A dropped `JoinHandle` does not abort its task, orphaning it on actor stop (panic or exit). See `run_done_task` in `session_actor.rs` and issue #1468.

### Testing Mechanics

- `TestConfigSandbox` serializes itself with a process-global mutex, so no `--test-threads=1` is needed; run under `cargo nextest` (per-test process isolation), never `cargo test`.
- Use `#[tokio::test(flavor = "multi_thread")]` for actor tests to catch runtime-specific races.
- Reap tests need a short TTL (`SessionRegistry::new_for_tests`, e.g. 50ms) and a round-trip command after `Unsubscribe`: `Unsubscribe` carries no reply, so a following `Get` is the only confirmation that the deadline is armed.
- Seed sessions via `registry.get_or_spawn + prompt`, not SSE stream collection.

## Related Issues

- **GitHub:** [issue #959](https://github.com/dobesv/harnx/issues/959) — AG-UI Phase 2: per-session actor control plane
- **GitHub:** [issue #1465](https://github.com/dobesv/harnx/issues/1465) — `get_or_spawn` handed out reaped actors' handles; the source of the ~1-in-10 503 flake in `rpc_session_prompt_returns_ack_and_persists_effect`
- **GitHub:** [issue #1468](https://github.com/dobesv/harnx/issues/1468) — orphan turn task after actor panic; double-writer window narrowed by `AbortOnDropHandle`
