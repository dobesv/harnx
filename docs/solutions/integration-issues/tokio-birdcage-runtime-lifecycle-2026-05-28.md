---
title: "Tokio Runtime Lifecycle for Birdcage Sandbox Activation"
date: 2026-05-28
category: integration-issues
problem_type: integration_issue
component: harnx-sandbox-run
root_cause: "Birdcage sandbox requires single-threaded context; tokio async runtime conflicts with namespace setup"
resolution_type: code_fix
severity: high
tags:
  - sandboxing
  - birdcage
  - tokio
  - runtime-lifecycle
  - namespaces
  - process-isolation
plan_ref: harnx-sandbox-run
---

## Problem

Birdcage sandbox uses Linux namespaces (user, mount, PID) that must be activated from a single-threaded context. A tokio multi-threaded runtime prevents proper namespace setup, causing sandbox activation to fail or behave incorrectly.

## Symptoms

- Sandbox activation panics or returns thread-safety errors when called from within async context
- Spawning sandboxed child processes hangs or returns `EPERM`/`EACCES`
- Namespace isolation incomplete — sandboxed process can see host filesystem

## Investigation Steps

1. Reviewed birdcage crate documentation — requires `fork()`-like semantics in single-threaded context
2. Tested `#[tokio::main]` pattern — runtime blocks namespace creation
3. Tested `current_thread` runtime dropped before birdcage setup — works
4. Separated concerns: hook dispatch needs async, sandbox activation does not

## Root Cause

Birdcage's `Sandbox::spawn()` internally calls `clone()` with namespace flags (`CLONE_NEWUSER`, `CLONE_NEWNS`, `CLONE_NEWPID`). The kernel's namespace implementation requires:

1. Single-threaded process at namespace creation time
2. No other threads holding references to resources being namespaced

A tokio runtime (even `current_thread`) creates internal state that conflicts with namespace setup. The runtime must be fully dropped before birdcage operations.

## Solution

Use a short-lived `current_thread` tokio runtime for async work, then drop it before sandbox setup:

**before** (broken):
```rust
#[tokio::main]
async fn main() -> Result<()> {
    let hook_env = hooks::run_hooks(&hook_defs, ...).await?;
    sandbox::setup_and_spawn(&cli, hook_env)?;  // FAILS: runtime still active
    Ok(())
}
```

**after** (correct):
```rust
fn main() -> Result<()> {
    let hook_env = if !hook_defs.is_empty() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let env = rt.block_on(hooks::run_hooks(&hook_defs, ...))?;
        drop(rt);  // CRITICAL: drop runtime before birdcage
        env
    } else {
        HashMap::new()
    };
    
    // Now safe: no tokio runtime active
    sandbox::setup_and_spawn(&cli, hook_env)?;
    Ok(())
}
```

**Key pattern**: 
1. Sync `main()` — no `#[tokio::main]`
2. Create `current_thread` runtime only when needed (hooks)
3. `rt.block_on()` for async work
4. `drop(rt)` before any birdcage call
5. Remaining execution is fully synchronous

## Why This Works

1. **Explicit runtime lifecycle**: `new_current_thread()` creates minimal async support without thread-pool overhead
2. **Clean state**: `drop(rt)` ensures all tokio internals (wakers, task queues) are released
3. **Namespace isolation**: Birdcage's `clone()` runs in pristine single-threaded process
4. **No contention**: Hook async work completes before sandbox setup begins

This pattern is appropriate when:
- Async work is bounded (hooks, I/O setup) and completes before sandbox lock
- Remaining execution is synchronous process management
- Multiple sandboxed children aren't spawned concurrently

For concurrent sandboxed spawns, use the **subprocess pattern** instead (see `cli-wrapper-sandboxing-for-tokio-servers-2026-04-28.md`).

## Prevention Strategies

**Test Cases:**
- Verify sandbox activation succeeds after hook runtime drop
- Test with both empty hooks (no runtime) and populated hooks (runtime created + dropped)
- Assert sandboxed child cannot access paths outside whitelist

**Best Practices:**
- When integrating sandbox crates with tokio, check if crate requires single-threaded context
- Prefer `current_thread` runtime when async work is preparatory, not ongoing
- Explicitly drop runtimes before calling into sandboxing layer
- Document the runtime lifecycle requirement at module boundary

**Code Review Checklist:**
- [ ] Is birdcage/spawn called after runtime drop?
- [ ] Is `current_thread` runtime used instead of multi-threaded?
- [ ] Is hook dispatch bounded (no post-sandbox async work)?
- [ ] Are tests present for both hook and non-hook paths?

## Related Issues

- **GitHub Issue:** [#575 — Standalone CLI Sandbox Wrapper](https://github.com/dobesv/harnx/issues/575)
- **Plan:** harnx-sandbox-run
- **Related Solution:** [cli-wrapper-sandboxing-for-tokio-servers-2026-04-28.md](cli-wrapper-sandboxing-for-tokio-servers-2026-04-28.md) — Subprocess wrapper pattern for tokio servers
- **Related Solution:** [../security-issues/environment-sanitization-bash-sandbox-2026-04-29.md](../security-issues/environment-sanitization-bash-sandbox-2026-04-29.md) — Environment control for sandboxed processes
