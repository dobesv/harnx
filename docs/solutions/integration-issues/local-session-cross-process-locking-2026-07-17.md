---
title: "Cross-Process File Locking for Local Session Concurrency"
date: 2026-07-17
category: "integration-issues"
problem_type: integration_issue
component: "session-lock"
root_cause: "local filesystem sessions lacked cross-process serialization"
resolution_type: code_fix
severity: high
tags:
  - file-locking
  - concurrency
  - cross-process
  - session
  - rust
  - re-entrancy
plan_ref: "harnx-848-local-session-concurrency"
---

# Cross-Process File Locking for Local Session Concurrency

## Problem


## Symptoms

- Two processes appending to same session could interleave YAML documents, producing invalid multi-document YAML
- Sequence number collisions when both processes derive `next_seq` from same file state
- Full-file `save()` rewrite truncating another process's concurrent append
- Agent-switch path deadlocking when `empty_session` attempted to re-acquire already-held lock
- Background title generation and compaction tasks racing with main agent loop
- TUI cancellation tests timing out due to unexpected blocking behavior

## Solution

Implemented `SessionLock` abstraction using `std::fs::File::lock`/`try_lock` (stable Rust 1.89+). Key design decisions:

1. **Lock file path**: `<session>.yaml.lock` sidecar file alongside session YAML
2. **Acquisition flow**: `try_acquire` first (non-blocking check), emit "Waiting for session lock…" notice, then `spawn_blocking(acquire)`
3. **Lock threading**: Pass `Option<&SessionLock>` through all in-loop mutation paths to avoid same-process re-entrant deadlock
4. **Background tasks**: Bounded `try_acquire` retry loops in `spawn_blocking` with fallback to skip/in-memory update

## Why This Works

- OS releases `flock` on file descriptor close (process crash safety)
- RAII guard lifetime ensures lock held for full mutation scope
- Threaded `Option<&SessionLock>` prevents same-process self-deadlock on non-reentrant `File::lock`
- Bounded retries avoid indefinite blocking while still converging on lock acquisition

## Key Learnings

### 1. `std::fs::File::lock`/`try_lock` API Shape on Rust 1.96.1

`TryLockError` is enum-shaped, not error-kind shaped:

```rust
match file.try_lock() {
    Ok(()) => ...,
    Err(TryLockError::WouldBlock) => ...,  // NOT e.kind() == ErrorKind::WouldBlock
    Err(TryLockError::Error(e)) => ...,
}
```

The `TryLockError::WouldBlock` variant indicates lock contention; `Error(e)` wraps actual I/O errors.

### 2. Re-Entrancy Discipline: Thread `Option<&SessionLock>` Through Dual Paths

`File::lock` is NOT re-entrant within a process. A second `File::open` + `lock` on the same path will block even if the same process already holds the lock.

**Pattern**: Every function callable both standalone AND from the agent loop must accept `Option<&SessionLock>`:
- `Some` → caller holds lock, skip acquisition
- `None` → standalone caller, self-acquire

**Affected call sites**:
- `save()` — `_lock` param
- `exit_agent_with_lock()` / `exit_session_with_lock()` — pass-through
- `empty_session_with_lock()` — NEW wrapper, base `empty_session()` delegates to `empty_session_with_lock(None)`

Never double-acquire same-process.

### 3. Background/Detached Task Appends

**Problem**: Background title generation and compaction are spawned via `tokio::spawn` after the turn completes, but may start before the runner lock drops. A blocking `SessionLock::acquire` would deadlock.

**Solution**: Bounded `try_acquire` retry loop inside `spawn_blocking`:

```rust
tokio::task::spawn_blocking(move || {
    let mut attempts = 0;
    let max_attempts = 20;  // ~2s total with 100ms sleeps
    loop {
        match SessionLock::try_acquire(&session_path) {
            Ok(Some(lock)) => return Ok(lock),
            Ok(None) => {
                attempts += 1;
                if attempts >= max_attempts { return Err(...); }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}).await?
```

**Critical constraints**:
- NEVER blocking `acquire` under a `config.write()` guard (would block async runtime)
- NEVER `try_acquire` loop on async thread (use `spawn_blocking`)
- Skip/in-memory fallback on timeout — compaction re-triggers next turn; title regenerates

### 4. Reload-on-Acquire Pitfalls

After acquiring lock, `reload_session_from_disk` runs before the turn starts. Several edge cases:

**Zero-byte session stub**: Empty file yields one YAML document (`null`). `SessionLogEntry::deserialize(null)` fails. Skip reload for zero-byte files.

**Preserve runtime append sink**: The double-Arc convention (`Arc<Arc<dyn SessionAppendSink>>` erased as `Arc<dyn Any>`) must survive reload. Downcast to inner `Arc<dyn SessionAppendSink>` and call `reset_seq_cache()`.

**Preserve in-memory Model**: Mock/dynamic clients (e.g., TUI tests) aren't in the static `clients` catalog. If `retrieve_model` fails for the session's `model_id`, fall back to preserving the existing runtime model.

**Sequence semantics**: Header = document 0 (implicit seq 0). First real entry = seq 1. Ensure header initialized before seq derivation.

### 5. Lock File Parent Directory

`SessionLock::acquire` creates the lock file, but `File::options().create(true)` does NOT create parent directories. Session dirs are lazily created.

**Fix**: `create_dir_all(lock_path.parent())` before opening lock file in BOTH `acquire` and `try_acquire`.

### 6. Testing: Full Workspace/e2e Catches Per-Crate Misses

- Re-entrancy deadlock (TUI cancellation tests)
- Model resolution failure on mock clients (TUI tests)

**Per project convention**: AGENTS.md mandates `cargo nextest` (never `cargo test`). Run full workspace suite for lock-related changes.

## Prevention Strategies

### Code Review Checklist
- [ ] Does mutation path acquire `SessionLock` or receive it via `Option<&SessionLock>`?
- [ ] Is `Option<&SessionLock>` threaded through to functions callable both standalone and in-loop?
- [ ] Do background tasks use `spawn_blocking` + bounded `try_acquire` retry?
- [ ] Is config write guard held ACROSS blocking file-lock syscall? (should be NO)
- [ ] Does reload handle zero-byte files and preserve runtime state?

### Best Practices
- Always use `try_acquire` + retry loop for background tasks, never blocking `acquire`
- Test lock acquisition under both `Some` (held) and `None` (self-acquire) paths
- Run full workspace e2e suite, not just per-crate tests

## Related Issues

- **Cross-reference**: [../nats-ha-lease.md](../nats-ha-lease.md) — NATS HA lease/fence pattern for distributed sessions. The local `.lock` file is the simpler single-machine equivalent; OS releases it on process death.
- **GitHub Issue**: [#848](https://github.com/dobesv/harnx/issues/848)
- **Plan notes**:
  - `c21fa894` — `TryLockError` enum shape
  - `d10bf179` — double-Arc `Session.runtime` convention
  - `8fa51be3` — exit-path lock threading
  - `9d9ad0f8` — parent dir before lock file open
  - `784dc01c` — zero-byte stub reload fix
  - `7abec798` — in-memory Model preservation
  - `14534bf3` — save() lock-guard bug fix
  - `eaabc72d` — empty_session re-entrancy deadlock
  - `3e873bcc` — background title generation bounded retry
