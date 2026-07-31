---
title: "MCP server background task supervision with restart semantics"
date: 2026-05-25
category: async-patterns
problem_type: logic_error
component: harnx-mcp-plans
root_cause: "RunningService::waiting consumes self, preventing direct select! alongside background task"
resolution_type: code_fix
severity: medium
tags:
  - tokio
  - spawn
  - JoinHandle
  - select!
  - supervision
  - mcp
  - rmcp
  - restart
plan_ref: harnx-mcp-plans-auto-cleanup
---

## Problem

rmcp's `RunningService::waiting(self)` consumes the service, making it impossible to `select!` directly on both the service future and a background task. Naive patterns either drop the background task JoinHandle (silent failures) or terminate the server when background task exits.

## Symptoms

- Background cleanup task failure takes down entire MCP server
- Double execution at startup (e.g., cleanup runs twice immediately)
- `dead_code` warnings during incremental feature development
- Nested async functions inaccessible from test module

## Investigation Steps

1. Attempted direct `select!` on `service.waiting()` and cleanup JoinHandle — compiler error: `waiting` consumes `self`, can't await twice
2. Tried spawning service.waiting() in task, keeping cleanup Handle — realized both needed to be JoinHandles for select! symmetry
3. Discovered `tokio::time::interval()` fires immediately on first `.tick().await`
4. Hit test visibility issue: async fn nested inside async fn invisible to `#[cfg(test)]` module

## Root Cause

**Pattern 1: Service consumption.** rmcp's `RunningService::waiting(self)` takes ownership. Pattern that works:

1. Spawn `service.waiting()` as its own task, returning a JoinHandle
2. Spawn cleanup task, returning another JoinHandle
3. Use `tokio::pin!` + loop + `select!` on both handles
4. When cleanup exits/panics: log error, spawn fresh cleanup task with cloned config
5. When service exits: break from loop, normal shutdown

**Pattern 2: Interval first tick.** `tokio::time::interval()` first tick is immediate. To run startup pass + subsequent periodic passes without duplication:

```rust
run_cleanup_pass(&dir, retention).await;  // startup pass
let mut interval = tokio::time::interval(period);
interval.tick().await;  // consume immediate tick
loop {
    interval.tick().await;
    run_cleanup_pass(&dir, retention).await;
}
```

**Pattern 3: spawn_blocking for fs I/O.** Walking directories and computing mtime inside `tokio::task::spawn_blocking` prevents blocking the async runtime.

**Pattern 4: Nested async fn visibility.** Extract them as top-level free functions for test access.

## Solution

**Main.rs supervision loop:**

```rust
let cleanup_dir = plans_dir.clone();
let mut cleanup_handle = tokio::spawn(server::cleanup_loop(plans_dir, retention_days));
let service_handle = tokio::spawn(async move { service.waiting().await });
tokio::pin!(service_handle);

loop {
    tokio::select! {
        result = &mut *service_handle => {
            result??;
            break;
        }
        result = &mut cleanup_handle => {
            if let Err(e) = result {
                eprintln!("[cleanup] task failed: {e}");
            }
            cleanup_handle = tokio::spawn(server::cleanup_loop(cleanup_dir.clone(), retention_days));
        }
    }
}
```

**Cleanup loop with correct interval handling:**

```rust
pub async fn cleanup_loop(dir: PathBuf, retention_days: u64) {
    let retention = Duration::from_secs(retention_days.saturating_mul(86_400));
    
    run_cleanup_pass(&dir, retention).await;
    
    let mut interval = tokio::time::interval(Duration::from_secs(86_400));
    interval.tick().await;
    
    loop {
        interval.tick().await;
        run_cleanup_pass(&dir, retention).await;
    }
}

async fn run_cleanup_pass(dir: &Path, retention: Duration) {
    for plan_dir in plan_dirs(dir) {
        let last_activity = tokio::task::spawn_blocking(move || {
            plan_last_activity(&plan_dir)
        }).await;
        // ... process ...
    }
}
```

## Why This Works

1. **Both tasks spawned**: `service.waiting()` wrapped in spawn returns a handle, enabling `select!` alongside cleanup handle.

2. **`tokio::pin!`**: Allows borrowing the JoinHandle mutably in select! after moving it into the pinned pointer.

3. **Loop with restart**: When cleanup panics/exits, spawn fresh task with cloned config. Server continues serving MCP requests.

4. **First tick consumption**: Immediate tick consumed before loop, preventing duplicate startup execution.

5. **Top-level async functions**: Extracted from nesting ensures `#[cfg(test)]` module can call them.

## Prevention Strategies

**Code Review Checklist:**
- [ ] `RunningService::waiting()` spawned into its own task
- [ ] Background task JoinHandle kept and supervised via `select!`
- [ ] Task failure logs error but continues service (unless explicit exit desired)
- [ ] `tokio::time::interval()` first tick consumed if startup pass runs separately
- [ ] Blocking fs I/O wrapped in `spawn_blocking`
- [ ] Async helper functions extracted to top-level for test visibility

**Best Practices:**
- Clone config/data before spawning background tasks that may need restart
- Use `tokio::pin!` for JoinHandles that need to be replaced in a loop
- Document intentional task detachment with `// fire-and-forget` comment
- Accept `dead_code` warnings when building incrementally — disappear once wired

**Test Cases:**
- Force cleanup task failure, verify server continues serving
- Add stale and fresh test data, run one pass, assert only stale deleted
- Verify only one startup pass when interval loop begins

## Related Issues

- **GitHub:** [issue #652](https://github.com/dobesv/harnx/issues/652) — Plan cleanup background task
