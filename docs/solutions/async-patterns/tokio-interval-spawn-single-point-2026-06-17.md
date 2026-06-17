---
title: "Single spawn point for background tasks covering all modes (TUI, CLI, serve)"
date: 2026-06-17
category: async-patterns
problem_type: logic_error
component: harnx
root_cause: "Background task spawn in mode-branch misses modes or duplicates execution"
resolution_type: code_fix
severity: medium
tags:
  - tokio
  - spawn
  - interval
  - serve
  - tui
  - cli
  - background-task
plan_ref: harnx-session-cleanup
---

## Problem

Background cleanup tasks were at risk of either missing execution in some modes (TUI/CLI/serve) or running multiple spawns if placed inside mode-specific branches. Additionally, `tokio::time::interval`'s first tick fires immediately, which can cause unintended double execution at startup if combined with an explicit pre-loop call.

## Symptoms

- Cleanup task only runs in TUI mode, not CLI or serve
- Cleanup task spawns multiple times (once per mode branch)
- Double execution at startup: explicit call + first interval tick both trigger cleanup
- Trivial invocations (`--list-models`, `--info`) unnecessarily spawn background tasks

## Investigation Steps

1. Traced `main.rs` execution flow: early-return flags (`--list-models`, `--info`) exit before TUI/CLI/serve branches
2. Identified mode divergence point: serve branch returns early at `serve::run()`, CLI branch installs event sink, TUI branch calls `start_interactive()`
3. Noted existing pattern: SIGINT watcher spawned once (~line 82) before mode branches
4. Investigated `tokio::time::interval` behavior: first `.tick().await` returns immediately (documented)
5. Discovered serve mode has no interactive transcript sink — `emit_agent_event` buffers but never delivers

## Root Cause

**Pattern 1: Spawn location.** Placing background task spawn inside a mode branch (e.g., TUI block) excludes other modes. Placing it in multiple branches duplicates spawns.

**Pattern 2: Interval first tick.** `tokio::time::interval().tick().await` fires immediately on first call. Combining an explicit startup call with `interval.tick().await` in a loop causes double execution at startup.

**Pattern 3: Serve-mode visibility.** Serve mode runs without an interactive transcript sink. Events emitted via `emit_agent_event` are buffered but never reach user-visible output.

## Solution

**spawn placement:** Place single spawn AFTER early-return flags but BEFORE serve branch check:

```rust
// Early-return flags (exit without real work)
if cli.list_models { /* ... */ return Ok(()); }
if cli.info { /* ... */ return Ok(()); }

// Single spawn covers TUI, CLI, AND serve
let cleanup_days = config.read().cleanup_inactive_sessions_days;
if let Some(days) = cleanup_days {
    if days > 0 {
        let config_clone = Arc::clone(&config);
        tokio::spawn(async move {
            // First tick fires immediately — cleanup runs at startup
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            loop {
                interval.tick().await;
                let stats = run_cleanup(&config_clone, days).await;
                emit_cleanup_summary(stats);
            }
        });
    }
}

// Mode branches diverge AFTER cleanup spawn
if let Some(addr) = cli.serve {
    return serve::run(config, addr).await;
}
// CLI branch...
// TUI branch...
```

**Interval first tick:** Use the immediate first tick for startup execution instead of explicit pre-loop call:

```rust
// Before (double execution):
run_cleanup(&config, days).await;  // Startup pass
let mut interval = tokio::time::interval(Duration::from_secs(3600));
loop {
    interval.tick().await;  // Fires immediately!
    run_cleanup(&config, days).await;  // Runs again
}

// After (single execution at startup):
let mut interval = tokio::time::interval(Duration::from_secs(3600));
loop {
    interval.tick().await;  // Immediate first tick = startup pass
    run_cleanup(&config, days).await;
}
```

**Serve-mode dual-path emission:**

```rust
fn emit_cleanup_summary(stats: CleanupStats) {
    if stats.sessions_removed == 0 { return; }
    let msg = format!("Note: cleaned up {} old sessions, {} disk freed",
        stats.sessions_removed, humanize_bytes(stats.bytes_freed));
    // TUI/CLI visibility via agent event sink
    emit_agent_event(AgentEvent::Notice(NoticeEvent::Info(msg.clone())));
    // Serve visibility via log (no transcript sink in serve mode)
    log::info!("{msg}");
}
```

## Why This Works

1. **Single spawn point before serve branch** ensures background task runs in all long-running modes (TUI, CLI, serve) while skipping trivial invocations that exit immediately via early-return flags.

2. **Interval's immediate first tick** provides startup execution without redundant explicit call. Loop begins with `tick().await`, executing cleanup once at startup then hourly thereafter.

3. **Dual-path emission** ensures cleanup summary reaches users in interactive modes (via buffered/flushed agent events) AND operators monitoring server logs in serve mode (via `log::info!`).

4. **Idempotent cleanup** makes double-execution harmless; second pass finds nothing to delete. But avoiding it reduces wasted I/O.

## Prevention Strategies

**Code Review Checklist:**
- [ ] Background task spawns placed BEFORE mode branches, AFTER early-return flags
- [ ] Interval tick position documented if relying on immediate first tick for startup
- [ ] Serve-mode output accounted for (log path or explicit suppression with comment)
- [ ] Trivial invocations (`--list-*`, `--info`) do NOT trigger background tasks

**Best Practices:**
- Trace execution flow for new background tasks: early returns → spawn → mode branches
- Document serve-mode visibility decisions inline (e.g., "serve has no transcript sink")
- Rely on interval's first tick for startup pass rather than explicit pre-loop call

**Test Cases:**
- Spawn occurs when config `cleanup_inactive_sessions_days > 0`
- Spawn skipped when config unset or `0`
- Cleanup runs exactly once at startup (no double execution)
- Trivial invocation (`--list-models`) exits without spawning

## Related Issues

- **Related Solution:** [mcp-server-background-task-supervision-2026-05-25.md](mcp-server-background-task-supervision-2026-05-25.md) — Interval first tick handling and supervision patterns for MCP server background tasks
- **GitHub:** [issue #847](https://github.com/dobesv/harnx/issues/847) — Automatic session cleanup feature
