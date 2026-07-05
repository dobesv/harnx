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
plan_ref: ag-ui-serve-control-plane
---

## Problem

Per-session actor in `harnx-serve` exhibited subtle race conditions:
1. **Prompt loss at completion boundary**: Prompts enqueued while a run was completing could be silently dropped.
2. **Reap race**: Actors could be reaped immediately after a new subscriber joined.
3. **Mid-loop injection data loss**: Multi-prompt injection drained all queued prompts but kept only one, silently discarding the rest.
4. **Test hangs**: SSE subscription streams drained to completion in tests blocked forever; `Notify::notify_waiters()` lost signals on multi-threaded runtimes.

## Symptoms

- **Prompt loss**: `session/prompt` during run completion never triggered a subsequent run.
- **Reap race**: After `Unsubscribe` + quick `Subscribe`, the actor sometimes disappeared from registry.
- **Data loss**: Enqueuing 2+ prompts during a run injected only the first; others silently dropped.
- **Test hangs**: E2E tests with `body.collect().await` on SSE streams hung indefinitely; tests using `Notify::notify_waiters()` deadlocked on multi-threaded runtimes.
- **Config panic**: Test helper `load_base_config_for_tests()` (which calls `std::env::set_current_dir` and expects `HARNX_CONFIG_DIR`) used in production paths caused thread-safety violations and panics.

## Root Cause

### 1. Missing COMPLETING-STATE Invariant

The actor had no explicit "completing" state to buffer prompts arriving during run finish. The `inject_tx.try_send` could return `Err(Full|Closed)` during the run-done transition, and without a `pending` queue, prompts were lost.

### 2. Reap Timer Race

On `Unsubscribe` when `subscribers == 0 && !running`, the actor armed a reap timer. A concurrent `Subscribe` canceled it, but the timer arm didn't re-check the subscriber count before removing from `DashMap`. A just-revived actor could be incorrectly reaped.

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

### 2. Reap Race Guard

On last unsubscribe (`subs == 0 && !running`), arm a reap `Sleep`. On `Subscribe`, cancel it. In the timer arm, **RE-CHECK** `subs == 0 && !running` before removing from `DashMap`:

```rust
// In reap timer arm
if self.subscribers == 0 && self.running.is_none() {
    // Safe to reap NOW — no race with subscribe
    self.should_exit = true;
}
```

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

Thread the server's real `Config` through `SessionRegistry::new(config)`:

```rust
pub struct SessionRegistry {
    base_config: Config,
    map: Arc<DashMap<SessionKey, SessionHandle>>,
}

impl SessionRegistry {
    pub fn new(base_config: Config) -> Self { ... }
}

// Actor clones and scopes per-run
fn prompt_config_for_agent_session(base_config: &Config, key: &SessionKey) -> GlobalConfig {
    let prompt_config = Arc::new(RwLock::new(base_config.clone()));
    let mut cfg = prompt_config.write();
    cfg.use_agent_by_name(&key.agent).expect("set actor agent");
    cfg.use_session(Some(&key.session)).expect("set actor session");
    prompt_config
}
```

**Test isolation**: `harnx-serve` tests mutate process env/cwd via `TestConfigSandbox` → MUST run `--test-threads=1` (serial). Parallel causes interference.

## Why This Works

- **Atomic drain**: No `.await` between run-done and pending check ensures no race window for prompt injection at completion boundary.
- **Re-check before reap**: Eliminates race between subscribe and timer arm; a just-revived actor survives.
- **One-per-round**: Preserves all queued prompts across rounds; no silent drops.
- **AbortSignal cancellation**: Preserves persistence semantics; partial state committed before unwind.
- **Bounded test reads**: Tests terminate; streams don't hang forever.
- **notify_one()**: Guarantees the next waiter wakes even if signal sent before registration.
- **Config threading**: Production paths never touch test-only env/chdir helpers; thread-safe config access.

## Prevention Strategies

### Test Cases

- **Prompt at finish boundary**: Enqueue prompt while run completing; assert subsequent run spawns with correct prompt.
- **Reap race**: Unsubscribe, sleep briefly, re-subscribe before TTL; assert actor survives and broadcast works.
- **Multi-prompt injection**: Enqueue 2+ prompts during run; assert BOTH land across successive tool rounds.
- **Cancel persistence**: Cancel mid-run; assert partial state persisted (persistence is NOT skipped).
- **SSE bounded read**: Test helper `read_sse_events_until(terminal_pred, timeout)` that returns partial, never hangs.
- **Notify signal**: Test with `#[tokio::test(flavor = "multi_thread")]`; verify `notify_one()` wakes waiter registered after signal.

### Code Review Checklist

- [ ] Does actor `select!` avoid `.await` between state transitions?
- [ ] Are timers/callbacks re-checking invariants before destructive actions?
- [ ] Is injection one-per-round (not drain-all-keep-first)?
- [ ] Does cancellation use `AbortSignal::set_ctrlc()`, not future drop?
- [ ] Do test SSE streams read bounded with timeouts, not `collect().await`?
- [ ] Is `notify_one()` used instead of `notify_waiters()` for single-waiter signaling?
- [ ] Are test-only helpers excluded from production paths (grep for `load_base_config_for_tests`, `set_current_dir`, env `.expect`)?
- [ ] Does `SessionRegistry`/actor hold the server's real `Config`, not reload from env?

### Testing Mechanics

- Run `harnx-serve` tests with `--test-threads=1` due to `TestConfigSandbox` env/cwd mutation.
- Use `#[tokio::test(flavor = "multi_thread")]` for actor tests to catch runtime-specific races.
- Seed sessions via `registry.get_or_spawn + prompt`, not SSE stream collection.

## Related Issues

- **GitHub:** [issue #959](https://github.com/dobesv/harnx/issues/959) — AG-UI Phase 2: per-session actor control plane
- **Related Solution:** [concurrent-session-prompt-isolation-2026-06-09.md](../logic-errors/concurrent-session-prompt-isolation-2026-06-09.md) — ACP concurrency isolation patterns
- **Related Solution:** [acp-idle-timeout-false-negative-2026-06-18.md](acp-idle-timeout-false-negative-2026-06-18.md) — Tokio select/patterns for timeout handling
