---
title: "Raw-mode terminal interrupt handling for Ctrl-C and Ctrl-D"
date: 2026-04-30
category: integration-issues
problem_type: integration_issue
component: cli-event-sink
root_cause: "crossterm raw mode bypasses terminal driver signal processing"
resolution_type: code_fix
severity: high
tags:
  - crossterm
  - raw-mode
  - ctrl-c
  - terminal
  - spawn_blocking
  - abort-signal
plan_ref: harnx-406-ctrl-c-interrupt
---

## Problem

In CLI one-shot mode with streaming markdown rendering, Ctrl-C and Ctrl-D were ignored. `crossterm::terminal::enable_raw_mode()` disables terminal driver signal processing — Ctrl-C becomes a raw key event rather than SIGINT, and Ctrl-D becomes a key event rather than EOF. The process never received the interrupt.

## Symptoms

- User presses Ctrl-C during streaming output — process continues running
- User presses Ctrl-D during streaming output — process continues running
- Terminal left in raw mode after certain error paths (no line buffering, no echo)
- Issue only reproduced with real terminal (tmux), not with `kill(SIGINT)`

## Investigation Steps

1. Traced the streaming path: `CliAgentEventSink::handle_markdown_chunk` calls `enable_raw_mode()` before rendering streaming chunks.
2. Verified crossterm raw mode behavior: terminal driver signal processing is disabled; keystrokes arrive as `Event::Key` rather than signals.
3. Confirmed existing `tokio::signal::ctrl_c()` handler does not fire — SIGINT is never delivered.
4. Identified that `poll_abort_signal` existed in spinner but was never called during streaming.
5. Recognized `spawn_blocking` requirement: `crossterm::event::poll` is blocking and would stall Tokio worker threads.
6. Discovered `JoinHandle::abort()` does NOT interrupt `spawn_blocking` threads — must use cooperative cancellation.

## Root Cause

`crossterm::terminal::enable_raw_mode()` disables the terminal driver's signal processing. When raw mode is active:

- Ctrl-C → `Event::Key(KeyEvent { code: Char('c'), modifiers: CONTROL })` — not SIGINT
- Ctrl-D → `Event::Key(KeyEvent { code: Char('d'), modifiers: CONTROL })` — not EOF

The existing code had no mechanism to poll crossterm events during raw mode. Additionally:

1. `run_abortable_spinner` had a blocking `poll_abort_signal` call inside an async function — stalls Tokio workers.
2. `handle_markdown_chunk` enabled raw mode before fallible setup (`MarkdownRender::init`, `terminal::size`) — failures left terminal corrupted.
3. `JoinHandle::abort()` cannot interrupt a `spawn_blocking` thread — requires cooperative stop mechanism.

## Solution

### 1. Added `RawModeKeyWatcher` with Cooperative Stop

```rust
// harnx-spinner/src/lib.rs
pub struct RawModeKeyWatcher {
    stop: AbortSignal,                              // Dedicated stop flag
    handle: tokio::task::JoinHandle<()>,            // spawn_blocking handle
}

impl RawModeKeyWatcher {
    pub fn stop(self) {
        self.stop.set_ctrlc();  // Thread exits on next poll iteration
    }
}

pub fn spawn_raw_mode_key_watcher(abort_signal: AbortSignal) -> Option<RawModeKeyWatcher> {
    let stop = create_abort_signal();
    let stop_clone = stop.clone();
    let handle = tokio::task::spawn_blocking(move || {
        loop {
            if abort_signal.aborted() || stop_clone.aborted() {
                break;
            }
            match poll_abort_signal(&abort_signal) {
                Ok(true) => break,
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
    Some(RawModeKeyWatcher { stop, handle })
}
```

Key design decisions:
- **`spawn_blocking`** (not `tokio::spawn`) because `crossterm::event::poll` is blocking.
- **Dedicated stop signal** separate from the operation's abort signal — `JoinHandle::abort()` cannot interrupt blocking I/O.
- **Singleton constraint**: only one watcher should exist at a time (crossterm event stream is process-global).

### 2. Scoped Watcher to Raw-Mode Window

```rust
// cli_event_sink.rs
fn handle_markdown_chunk(&mut self, text: &str) -> anyhow::Result<()> {
    if !self.raw_mode_active {
        enable_raw_mode()?;
        self.raw_mode_active = true;
        self.key_watcher = spawn_raw_mode_key_watcher(self.abort_signal.clone());
    }
    // ... rendering ...
}

fn cleanup(&mut self) -> anyhow::Result<()> {
    if self.raw_mode_active {
        if let Some(watcher) = self.key_watcher.take() {
            watcher.stop();  // Signals thread to exit within 25ms
        }
        disable_raw_mode()?;
        self.raw_mode_active = false;
    }
    // ...
}
```

### 3. Added Cleanup Guard for Fallible Setup

```rust
// cli_event_sink.rs
if self.render.is_none() {
    let init_result = (|| -> anyhow::Result<()> {
        self.render = Some(MarkdownRender::init(self.render_options.clone())?);
        self.columns = crossterm::terminal::size()?.0;
        Ok(())
    })();
    if let Err(e) = init_result {
        let _ = self.cleanup();  // Disable raw mode before propagating error
        return Err(e);
    }
}
```

### 4. Removed Blocking Poll from Async Spinner

`run_abortable_spinner` no longer calls `poll_abort_signal`. It checks `abort_signal.aborted()` (non-blocking atomic) and uses `tokio::time::sleep` (async-compatible).

### 5. Added Tmux-Based E2E Test

```rust
// tests/interrupt_e2e.rs
#[test]
fn interrupt_cmd_raw_mode_ctrlc() -> Result<()> {
    let tmux = spawn_oneshot_in_tmux(&paths, &harnx_bin, "hello", &repo_root)?;
    tmux.wait_for_contains("Thinking", Duration::from_secs(10))?;
    tmux.send_keys(&["C-c"])?;  // Real terminal key event
    let nonzero = wait_for_cmd_exit(&tmux, Duration::from_secs(5))?;
    assert!(nonzero);
    Ok(())
}
```

This test exercises the actual raw-mode Ctrl-C path through crossterm's event stream — unlike `kill(SIGINT)` tests which bypass the terminal.

## Why This Works

1. **`spawn_blocking`** isolates blocking `crossterm::event::poll` from Tokio's worker pool — no executor stalls.
2. **Dedicated stop signal** enables clean shutdown of `spawn_blocking` threads that `JoinHandle::abort()` cannot interrupt.
3. **Scoped lifecycle** (start on raw-mode enable, stop on disable) ensures no concurrent crossterm readers.
4. **Cleanup guard** ensures terminal is never left corrupted on init failures.
5. **Tmux test** verifies the actual user-reported issue path, not a synthetic approximation.

## Prevention Strategies

**Test Cases:**
- E2E test `interrupt_cmd_raw_mode_ctrlc` sends real Ctrl-C through tmux terminal
- Verify non-zero exit after Ctrl-C during streaming
- Test covers raw-mode path that `kill(SIGINT)` bypasses

**Best Practices:**
- Always use `spawn_blocking` for blocking crossterm operations
- Use cooperative cancellation (dedicated AbortSignal or channel) for `spawn_blocking` threads
- Scope raw-mode setup/teardown tightly with guard patterns
- Test terminal behavior with real PTY (tmux), not just signal injection

**Code Review Checklist:**
- [ ] Is `crossterm::event::poll` inside `spawn_blocking`, not `tokio::spawn`?
- [ ] Does `spawn_blocking` thread check a stop signal each iteration?
- [ ] Is raw mode disabled on all error paths after being enabled?
- [ ] Are there unit/E2E tests that exercise raw-mode interrupt handling?

## Related Issues

- **GitHub:** [#406](https://github.com/dobesv/harnx/issues/406) — CTRL-C and CTRL-D ignored in terminal mode
- **Related Solution:** [git-backed-local-history-rollback-2026-04-26.md](../logic-errors/git-backed-local-history-rollback-2026-04-26.md) — `spawn_blocking` pattern for gix I/O
