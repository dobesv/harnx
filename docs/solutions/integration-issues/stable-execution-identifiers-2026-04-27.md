---
title: "Stable execution identifiers via UUID tempdir pattern in bash MCP server"
date: 2026-04-27
category: "integration-issues"
problem_type: integration_issue
component: "harnx-mcp-bash"
root_cause: "PID reuse and counter-based identifiers created unstable, collision-prone execution tracking"
resolution_type: code_fix
severity: high
tags:
  - tempfile
  - uuid
  - execution-tracking
  - bash-mcp
  - validate_path
plan_ref: "issue-352-bash-mcp-refactor"
---

## Problem

The bash MCP server used OS PIDs (`u32`) as HashMap keys for tracking spawned processes and `AtomicU64` counters for generating log file names. PIDs are reused by the OS after process exit, causing collisions. Counter-based log names (`bg-{seq}.log`, `exec-{seq}.stdout.log`) required global state, had potential for wrap-around collisions, and were not stable external identifiers. Additionally, `spawn` merged stdout/stderr into a single file while `exec` kept them separate, creating inconsistency.

## Symptoms

```
- spawned HashMap keyed on PID — PID reuse could orphan or misidentify processes
- spawn returned `pid: <u32>` field that clients had to track
- spawn wrote merged stdout+stderr to single file; exec used separate files
- wait output format differed from exec: missing filtering params, different field
  structure
- read_exec_log required clients to construct file paths manually
- Counter state persisted across server restarts, no uniqueness guarantee
```

## Investigation Steps

Reviewed `BashServerInner` struct in `crates/harnx-mcp-bash/src/server.rs`. Found `spawned: Mutex<HashMap<u32, SpawnedProcess>>` keyed on PID, plus `spawn_counter` and `exec_counter` AtomicU64 fields. Traced `spawn_impl` to see merged log file creation via `bg-{seq}.log` pattern. Compared `wait_impl` response formatting against `exec_command_impl` — confirmed field mismatches and missing filter params.

Noted that `tempfile` crate was already a workspace dependency. Investigated `tempfile::Builder::prefix().tempdir_in()` pattern for generating unique directories whose names serve as stable identifiers.

First attempt at bulk refactor via regex substitution failed — produced duplicate `ExecCommandParams` definition and schema drift. Reverted to HEAD and used targeted Edit-tool replacements section by section.

## Root Cause

1. **PID reuse vulnerability**: OS reuses PIDs after process exit. A long-running server could see the same PID for different processes, corrupting the `spawned` map.

2. **Counter brittleness**: `AtomicU64` counters required global state in `BashServerInner`, added complexity, and were semantically disconnected from the actual execution lifecycle.

3. **Format inconsistency**: `spawn` merged streams; `exec` kept them separate. `wait` lacked grep/head_lines params and used different response schema.

4. **Path construction burden**: `read_exec_log` accepted raw `path: String`, forcing clients to know internal log directory structure.

## Solution

### Per-execution directory pattern

Replaced counters with `tempfile::Builder` to create unique directories per execution. The directory name becomes the stable `execution_id`.

```rust
fn next_exec_dir(&self) -> Result<tempfile::TempDir, ErrorData> {
    tempfile::Builder::new()
        .prefix("exec-")
        .tempdir_in(&self.inner.log_dir)
        .map_err(|err| internal_error(format!("failed to create exec directory: {err}")))
}
```

Usage in `exec_command_impl` and `spawn_impl`:

```rust
let exec_dir = self.next_exec_dir()?;
let stdout_log_path = exec_dir.path().join("stdout.log");
let stderr_log_path = exec_dir.path().join("stderr.log");
let execution_id = exec_dir.path()
    .file_name()
    .unwrap()
    .to_string_lossy()
    .into_owned();
// Keep TempDir alive until response built (exec) or persist via into_path() (spawn)
```

### Struct refactoring

Changed `spawned` HashMap key from `u32` to `String`:

```rust
// Before
spawned: Mutex<HashMap<u32, SpawnedProcess>>,

// After
spawned: Mutex<HashMap<String, SpawnedProcess>>,
```

Removed `SpawnedProcess.log_path` and added `stdout_log_path`/`stderr_log_path`. Removed `spawn_counter` and `exec_counter` from `BashServerInner`.

### Independent stream rendering with clear markers

Added `render_stream_block` and `render_streams_block` for per-stream independent truncation:

```rust
fn render_stream_block(
    name: &str,
    content: &str,
    truncate_opts: &TruncateOpts,
    log_hint: Option<(&str, &Path)>,
) -> String {
    let sanitized = sanitize_output_text(content);
    if sanitized.is_empty() {
        return format!("===== {name} (empty) =====");
    }

    let (truncated, _) = truncate_output(&sanitized, truncate_opts);
    let mut block = format!("===== {name} =====\n{truncated}");

    // Add truncation hint if content was reduced
    if truncated.len() < sanitized.len() {
        if let Some((execution_id, log_path)) = log_hint {
            let _ = write!(
                block,
                "\n\n[{name} truncated from {} to {}. Use max_output_bytes, head_lines, or tail_lines to see more; full log via read_exec_log: execution_id={execution_id}, stream={name} ({})]",
                format_size(sanitized.len()),
                format_size(truncated.len()),
                log_path.display()
            );
        }
    }

    let _ = write!(block, "\n===== /{name} =====");
    block
}
```

Result format:

```
===== stdout =====
<content>
===== /stdout =====
===== stderr (empty) =====
```

### read_exec_log refactor to use execution_id

Changed params from `path: String` to `execution_id: String` + `stream: String`:

```rust
struct ReadExecLogParams {
    execution_id: String,
    stream: String,  // "stdout" or "stderr"
    // ...filter params...
}
```

Path construction now happens server-side:

```rust
// Validate stream parameter
if params.stream != "stdout" && params.stream != "stderr" {
    return Err(ErrorData::invalid_params(
        format!("stream must be 'stdout' or 'stderr', got '{}'", params.stream),
        None,
    ));
}

// Construct absolute path: log_dir/execution_id/stream.log
// We pass the absolute path string so validate_path uses canonicalize() correctly
// (passing a relative string would resolve against cwd, not log_dir).
let abs = self.inner.log_dir
    .join(&params.execution_id)
    .join(format!("{}.log", params.stream));
let path = validate_path(
    abs.to_string_lossy().as_ref(),
    std::slice::from_ref(&self.inner.log_dir),
)?;
```

## Why This Works

**UUID-based tempdir names** (`exec-<random>`) are globally unique, stable for the execution lifetime, and require no global counter state. The `tempfile` crate handles collision avoidance internally.

**String execution_id** as HashMap key is stable and opaque — clients don't need to know it's a directory name.

**Independent stream files** (`stdout.log`, `stderr.log`) allow per-stream filtering, truncation, and retrieval via `read_exec_log` with `stream` param.

**Marker convention** (`===== stdout =====` / `===== /stdout =====`) lets LLM/agent readers unambiguously parse output blocks. Empty streams render as `===== stderr (empty) =====` (compact, no end marker).

**Server-side path construction** in `read_exec_log` hides log directory layout from clients and ensures `validate_path` security check works correctly.

## Gotcha: validate_path requires absolute paths

The `read_exec_log_impl` implementation constructs an absolute path before passing to `validate_path`:

```rust
let abs = self.inner.log_dir.join(&params.execution_id).join(format!("{}.log", params.stream));
let path = validate_path(
    abs.to_string_lossy().as_ref(),  // absolute path string
    std::slice::from_ref(&self.inner.log_dir),
)?;
```

Reason: `validate_path` internally uses `std::fs::canonicalize()` on the input path. If a relative path string is passed, canonicalize resolves it against the **current working directory**, not `log_dir`. This causes "Cannot resolve path" errors or incorrect path validation.

Always construct the absolute path before calling `validate_path` when the root is not cwd.

## Advisory: server.rs file size

CodeScene flagged server.rs growing from 1549 → 1664 lines, along with increased Low Cohesion metric (12 → 13 responsibilities). This is **not a CI gate** (cs delta exited 0). The file was already over-threshold before this change.

**Future work**: Split `server.rs` into sub-modules (e.g., `exec.rs`, `spawn.rs`, `render.rs`, `tool_defs.rs`) to reduce file size and improve cohesion. Out of scope for this refactor.

## Prevention Strategies

**Test cases:**
- Verify `execution_id` is present in all exec/spawn/wait/terminate responses
- Test that `wait` accepts same filtering params as `exec`
- Test `read_exec_log` with `execution_id` + `stream` params for both stdout and stderr
- Test `read_exec_log` rejects invalid stream values
- Verify spawned processes tracked by String execution_id, not PID

**Code review checklist:**
- [ ] No PID-based HashMap keys for long-lived state
- [ ] `validate_path` called with absolute path strings
- [ ] TempDir kept alive until response built (exec) or converted with `into_path()` (spawn)
- [ ] Empty streams rendered as `===== name (empty) =====` without end marker

**Monitoring:**
- Consider alerting if `spawned` HashMap grows unbounded (orphaned entries detection)

## Related Issues

- **Issue:** [#352](https://github.com/example/harnx-2/issues/352) — Bash tool inconsistencies fixup
- **CodeScene advisory:** File size 1549→1664, cohesion 12→13 — not blocking, tracked for future refactor
