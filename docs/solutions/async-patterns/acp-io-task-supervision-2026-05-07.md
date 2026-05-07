---
title: "Supervised background task in LocalSet command loop"
date: 2026-05-07
category: async-patterns
problem_type: logic_error
component: harnx-acp-client
root_cause: "dropped JoinHandle from spawn_local allowed command loop to continue after I/O task death"
resolution_type: code_fix
severity: medium
tags:
  - tokio
  - spawn_local
  - LocalSet
  - JoinHandle
  - select!
  - supervision
  - acp
plan_ref: issue-71-acp-io-task-tracking
---

## Problem

ACP client used `tokio::task::spawn_local` to run I/O loop but immediately dropped the `JoinHandle`. Command loop continued indefinitely even when underlying subprocess crashed, causing silent failures.

## Symptoms

- ACP commands hang indefinitely after subprocess crashes
- No error messages or warnings when transport dies unexpectedly
- In-flight `oneshot::Receiver` calls return `RecvError` ("disconnected")
- Client appears functional but all commands timeout

## Investigation Steps

1. Reviewed `client.rs` — found `spawn_local(...)` called without binding to variable
2. Understood `JoinHandle` drops immediately, detaching I/O task from supervision
3. Recognized: if subprocess crashes, I/O task exits silently while command loop keeps running
4. Traced oneshot channels: senders dropped when I/O loop breaks → receivers get `RecvError`
5. Identified need for supervision: command loop must observe I/O task termination

## Root Cause

`tokio::task::spawn_local` returns a `JoinHandle` that represents the spawned task. When dropped:
- Task continues running but becomes detached (unobservable)
- No way to detect task termination from outside
- Command loop has no signal that transport layer died

In this case, I/O task handles subprocess communication. If subprocess crashes:
- I/O task terminates
- `JoinHandle` already dropped → no notification
- Command loop continues accepting commands that can never succeed

## Solution

Capture `JoinHandle` and supervise it in the command loop using `tokio::select!`:

**Before:**
```rust
LocalSet::new().run_until(async {
    tokio::task::spawn_local(io_loop(subprocess));

    while let Some(cmd) = command_stream.recv().await {
        // process commands - no visibility into io_loop health
    }
});
```

**After:**
```rust
LocalSet::new().run_until(async {
    let mut io_handle = tokio::task::spawn_local(io_loop(subprocess));

    loop {
        tokio::select! {
            Some(cmd) = command_stream.recv() => {
                // process command
            }
            _ = &mut io_handle => {
                tracing::warn!("I/O task exited unexpectedly");
                break;
            }
        }
    }
});
```

Additional safety: `kill_on_drop(true)` on subprocess ensures cleanup even without explicit shutdown.

## Why This Works

1. **`&mut io_handle` in select**: Borrows handle mutably, allowing repeated polls without consuming it. Each select iteration checks if I/O task completed.

2. **Early termination detection**: If I/O loop exits (normally or due to crash), select branch fires immediately. Command loop can log warning and stop accepting new commands.

3. **Channel propagation**: When loop breaks, in-flight `oneshot::Sender`s drop → callers receive `RecvError`, getting proper error signal instead of hanging.

4. **`kill_on_drop(true)`**: Ensures subprocess terminates when handle drops, even during panics or early returns.

## Prevention Strategies

**Code Review Checklist:**
- [ ] `spawn_local` results captured when task lifecycle matters
- [ ] Background task termination observable (JoinHandle supervised or abortable)
- [ ] `select!` used to supervise multiple async lifetimes in one loop

**Best Practices:**
- Always capture `JoinHandle` from `spawn`/`spawn_local` unless fire-and-forget is intentional
- Use `tokio::select!` when coordinating main loop with background task lifetime
- Set `kill_on_drop(true)` on child processes for reliable cleanup
- Document why a task can be safely detached (if intentionally dropped)

**Test Cases:**
- Simulate subprocess crash, verify command loop exits
- Verify in-flight commands receive errors, not hangs
- Test: I/O task panic still triggers cleanup

## Related Issues

- **GitHub:** [issue #71](https://github.com/dobesv/harnx/issues/71) — ACP I/O task tracking
- **Related Solution:** [raw-mode-ctrl-c-interrupt-2026-04-30.md](../integration-issues/raw-mode-ctrl-c-interrupt-2026-04-30.md) — `JoinHandle::abort()` cancellation patterns for `spawn_blocking`
