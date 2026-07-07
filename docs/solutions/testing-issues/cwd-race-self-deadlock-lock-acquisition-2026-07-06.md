---
title: "Self-deadlock from acquiring lock inside helper when callers already hold it"
date: 2026-07-06
category: "testing-issues"
problem_type: logic_error
component: "harnx-serve test harness"
root_cause: "non-reentrant std::sync::Mutex acquired inside function while callers hold same lock"
resolution_type: code_fix
severity: high
tags:
  - deadlock
  - testing
  - rust
  - mutex
  - process-global-state
  - cwd
  - nextest
plan_ref: "harnx-959-followups"
---

# Self-deadlock from acquiring lock inside helper when callers already hold it

## Problem

A test helper function that accessed process-global state (`std::env::set_current_dir`) caused a race condition with concurrent tests. An intermediate fix attempted to guard the shared state with an existing `std::sync::Mutex` — but ~16 call sites already held that same mutex guard for the lifetime of a `TestConfigSandbox`, causing immediate self-deadlock when the helper tried to acquire it.

## Symptoms

```text
- 16 tests hung for 60+ seconds under `cargo nextest run`
- Tests killed with SIGTERM after timeout
- Wrapper `timeout` command exited with code 124
- nextest output: "has been running for over 60 seconds" before kill
- No panic messages or error output — pure hang
```

**Detection tip:** Self-deadlock manifests as a HANG, not a panic. Run nextest under `timeout` and treat exit 124 as the signal for lock-order deadlock.

## Investigation Steps

1. Identified original race: `load_base_config_for_tests()` used `std::env::set_current_dir()` to switch to config directory, load config, then restore cwd. Any concurrent test reading cwd during this window would see inconsistent state.

2. Intermediate fix: Wrap the cwd manipulation in `TEST_CONFIG_DIR_LOCK.lock().unwrap()` — the same `LazyLock<StdMutex<()>>` used to serialize `TestConfigSandbox` construction.

3. Verified nextest enforcement via `harnx_core::require_nextest()` — this repo mandates per-test process isolation.

4. Ran `cargo nextest run -p harnx-serve` — 16 tests hung, killed after 236 seconds.

5. Traced call sites: `TestConfigSandbox::new()` acquires `TEST_CONFIG_DIR_LOCK` guard and holds it for the entire sandbox lifetime. Call sites use pattern:
   ```rust
   let sandbox = TestConfigSandbox::new();  // holds lock
   let config = load_base_config_for_tests();  // tries to acquire same lock → deadlock
   ```

6. Recognized `std::sync::Mutex` is NOT reentrant — same thread acquiring same lock twice deadlocks.

7. **Root fix:** Eliminate shared mutable state entirely instead of guarding it. Made `Config::load_from_file(&Path)` public and rewrote helper to load from explicit path: `$HARNX_CONFIG_DIR/config.yaml`. No cwd mutation → no lock needed → no deadlock.

## Root Cause

Two distinct issues:

1. **Process-global state mutation (`set_current_dir`)** creates inherent race conditions across threads/processes. Under `nextest` per-test process isolation, tests run in separate processes, but process-global state (cwd, env vars) is still shared within each process — concurrent test threads within the same process can still race.

2. **Self-deadlock from lock acquisition inside helper** — the mutex pattern `ENV_LOCK.lock()` inside a function is safe ONLY if no caller already holds that lock. With non-reentrant `std::sync::Mutex`, same-thread re-acquisition deadlocks. The helper had no way to know callers held the lock.

## Solution

**Eliminate process-global state mutation instead of guarding it:**

```rust
// BEFORE (race + deadlock hazard):
pub(crate) fn load_base_config_for_tests() -> Config {
    let _guard = TEST_CONFIG_DIR_LOCK.lock().unwrap();  // DEADLOCK: callers hold this!
    let prev = std::env::current_dir().expect("cwd");
    let root = std::env::var("HARNX_CONFIG_DIR").expect("HARNX_CONFIG_DIR set");
    std::env::set_current_dir(&root).expect("switch cwd");
    let result = futures::executor::block_on(Config::init(WorkingMode::Cmd, false, vec![]));
    std::env::set_current_dir(prev).expect("restore cwd");
    result.expect("load config")
}

// AFTER (no cwd mutation, no lock needed, no deadlock):
pub(crate) fn load_base_config_for_tests() -> Config {
    let root = std::env::var_os("HARNX_CONFIG_DIR").expect("HARNX_CONFIG_DIR set");
    let config_file = std::path::PathBuf::from(&root).join("config.yaml");
    Config::load_from_file(&config_file).expect("load config")
}
```

Also made `Config::load_from_file(&Path)` public (was `pub(crate)`) to enable explicit-path loading.

Added comment explaining why lock must NOT be reintroduced:

```rust
// This deliberately does NOT touch the process-global current directory and
// does NOT acquire `TEST_CONFIG_DIR_LOCK`. `load_from_file` takes an explicit
// path, so there is no shared mutable state to guard, and acquiring the lock
// here would self-deadlock: callers routinely hold a live `TestConfigSandbox`
// (which owns the same std `Mutex` guard for its whole lifetime) while calling
// this function.
```

## Why This Works

**Explicit-path API removes the race class:** By passing the config file path explicitly to `load_from_file`, the function never needs to observe or mutate process-global cwd. No shared mutable state means no serialization needed, no lock needed, no deadlock possible.

**State elimination > lock serialization:** The intermediate fix tried to serialize access to process-global state, but:
- Added complexity (lock acquisition)
- Introduced self-deadlock hazard
- Still had overhead of cwd syscalls

The root fix removes the problem entirely by eliminating the shared state dependency.

## Prevention Strategies

**Before adding in-function lock acquisition:**
- Audit all call sites — does any caller already hold this lock?
- If yes, either: (a) refactor to eliminate shared state, or (b) pass lock guard as parameter
- Prefer eliminating shared state over serializing it

**Process-global state in tests:**
- Avoid `std::env::set_current_dir()`, `std::env::set_var()` in test helpers
- Prefer explicit parameters or test-local temp directories
- If mutation is unavoidable: single shared lock, all users acquire same lock
- Document why lock exists and what callers must do

**nextest deadlock detection:**
```bash
timeout 120 cargo nextest run -p <crate> --stress-count=5
# Exit 124 = timeout = likely lock-order deadlock or infinite wait
```

**Code Review Checklist:**
- [ ] Does helper acquire any locks? Which ones?
- [ ] Do any callers hold those locks while calling helper?
- [ ] Can shared mutable state be eliminated instead of guarded?
- [ ] Is `std::sync::Mutex` used? (non-reentrant — self-deadlock on re-acquire)
- [ ] For process-global state: is elimination possible? If not, single lock?

## Related Issues

- **PR:** harnx-959-followups branch (F3)
- **Component:** `crates/harnx-serve/src/session_actor.rs` — `load_base_config_for_tests()`
- **Component:** `crates/harnx-runtime/src/config/loader_split.rs` — `Config::load_from_file()`
- **Related pattern:** `docs/solutions/workflow-issues/sandbox-project-root-pseudo-vars-2026-06-02.md` — same env-mutation locking pattern (different deadlock mode: per-module locks don't serialize)
