---
title: "CLI Wrapper Binary Pattern for Sandboxing Tokio Servers with Birdcage"
date: 2026-04-28
category: integration-issues
problem_type: integration_issue
component: harnx-mcp-bash
root_cause: "Sandboxing crate requires single-threaded context; tokio multi-threaded runtime incompatible"
resolution_type: code_fix
severity: high
tags:
  - sandboxing
  - birdcage
  - tokio
  - cli-wrapper
  - process-isolation
plan_ref: bash-sandboxing-birdcage
---

## Problem

Birdcage, a Rust sandboxing crate, requires a single-threaded execution context to activate its sandbox. Tokio's multi-threaded runtime (the default with `#[tokio::main]`) spawns the server across multiple threads, making direct in-process sandbox activation impossible.

## Symptoms

- Calling `Birdcage::new()` or `sandbox.lock()` from within a tokio multi-threaded runtime panics or fails with thread-safety errors
- Attempting to sandbox individual commands from the server process has no effect or causes runtime crashes
- Documentation for birdcage explicitly requires single-threaded context

## Investigation Steps

1. Reviewed birdcage crate documentation and source — confirmed single-threaded requirement
2. Evaluated forcing tokio to single-threaded runtime — rejected due to performance impact and breaking async patterns
3. Explored process-level isolation patterns — CLI wrapper binary emerged as cleanest architecture
4. Tested wrapper binary approach: server spawns helper process that activates sandbox before exec'ing bash

## Root Cause

Birdcage uses thread-local state and signal handlers that must run in a controlled single-threaded context. Tokio's work-stealing scheduler moves tasks between threads, violating these constraints. The sandbox cannot be "activated" in one thread while other threads may spawn unsandboxed work.

## Solution

**CLI Wrapper Binary Pattern:**

1. Create a separate binary (`harnx-mcp-bash-sandbox-run`) that:
   - Parses sandbox configuration from CLI arguments
   - Activates birdcage sandbox *before* spawning the command
   - Execs the target command inside the sandbox

2. The main MCP server:
   - Does NOT call birdcage directly
   - Spawns the wrapper binary with sandbox configuration as arguments
   - Existing process management (KillOnDrop, ProcessGroup, timeouts) works transparently

**Architecture:**
```
[MCP Server (tokio multi-threaded)]
        |
        | spawn wrapper process
        v
[harnx-mcp-bash-sandbox-run (single-threaded)]
        |
        | activate birdcage sandbox
        v
[bash command (sandboxed)]
```

**Key Implementation Details:**

- Wrapper binary exists only on Unix (`#[cfg(unix)]`)
- Server discovers wrapper path via `current_exe().parent().join("harnx-mcp-bash-sandbox-run")`
- Fail-closed startup: server refuses to start if wrapper missing (unless `--no-sandbox` explicitly set)
- Process management patterns (KillOnDrop, ProcessGroup) apply to wrapper PID, signals propagate correctly

## Why This Works

The wrapper binary runs in its own process with a single thread. Birdcage can safely activate its sandbox in this context. When the wrapper spawns bash, bash is already confined. The MCP server never directly activates sandbox state — it only orchestrates the wrapper process.

This preserves:
- Tokio multi-threaded performance in the main server
- All existing process management patterns
- Clean separation between server logic and sandbox configuration

## Birdcage 0.8 API Quirks

**Exception Variants:**
- `Exception::Read` — read-only access
- `Exception::WriteAndRead` — read-write access (NOT `Write` alone)
- `Exception::ExecuteAndRead` — execute + read (NOT `Execute`)
- `Exception::FullEnvironment` — full environment access (required for basic process spawning)
- `Exception::Networking` — allow network access

**No `current_dir` on `birdcage::process::Command`:**
Birdcage's `Command` wrapper does not expose `current_dir()`. Workaround used in initial implementation:
```rust
let mut wrapped = Command::new("/usr/bin/env");
wrapped.arg("--chdir");
wrapped.arg(working_dir);
wrapped.arg(&command[0]);
```

**Portability Note:** `env --chdir` is GNU-only, fails on Alpine/Busybox. Future fix should explore alternatives.

## PID Model

Birdcage spawns a PID-namespace init process that hosts the bash process. The wrapper binary does NOT exec away — it stays as the sandbox supervisor. This means:
- `process_wrap::KillOnDrop` works on the wrapper PID
- `ProcessGroup::leader()` propagates signals to nested bash
- Server can terminate wrapper, which terminates sandboxed process tree

## Per-Call Path Semantics

The `inputs` and `outputs` tool parameters control sandbox exceptions:

| Parameter | `None` | `Some([])` | `Some([paths...])` |
|-----------|--------|------------|---------------------|
| `inputs` | Default readable paths | No root read access | Only listed paths readable |
| `outputs` | Default writable roots | No root write access | Only listed paths writable |

**Interaction:** `inputs=Some([])` + `outputs=Some([])` = roots absent entirely from sandbox (deny-by-default).

**Validation:** All paths validated against MCP roots BEFORE forwarding to wrapper. Prevents agent-controlled path escapes.

## Fail-Closed Security Policy

Server startup behavior:
- Sandbox enabled + helper found → normal sandboxed operation
- Sandbox enabled + helper missing → **bail with error** (refuse to start)
- `--no-sandbox` explicitly passed → unsandboxed operation (opt-in)

Never silently degrades to unsandboxed mode.

## Test Isolation Limitation

`CARGO_BIN_EXE_*` is only set for integration tests, not unit tests in `mod tests`. Workaround:
```rust
fn sandbox_run_test_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/harnx-mcp-bash-sandbox-run")
}
```

## Prevention Strategies

**For Future Sandboxing Implementations:**
- Check sandboxing crate runtime requirements early (single-threaded? signal handlers?)
- Evaluate wrapper binary pattern when in-process activation is impossible
- Validate all agent-controlled paths against policy roots before adding sandbox exceptions
- Implement fail-closed startup for all security-critical features

**Code Review Checklist:**
- [ ] Does sandboxing crate require special runtime context?
- [ ] Is fail-closed startup implemented for security features?
- [ ] Are agent-supplied paths validated against allowed roots?
- [ ] Does process termination propagate correctly through sandbox wrapper?

## Related Issues

- **GitHub Issue:** [#360 — Good sandboxing for bash commands](https://github.com/example/repo/issues/360)
- **Plan:** bash-sandboxing-birdcage
- **Commit:** f6a0449 — Add filesystem sandboxing to harnx-mcp-bash using birdcage
