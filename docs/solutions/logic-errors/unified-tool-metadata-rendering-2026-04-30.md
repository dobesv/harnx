---
title: "Unified response metadata fields across bash MCP exec/spawn/wait/terminate tools"
date: 2026-04-30
category: "logic-errors"
problem_type: logic_error
component: "harnx-mcp-bash"
root_cause: "inconsistent metadata field ordering and missing fields across tool responses"
resolution_type: code_fix
severity: medium
tags:
  - metadata
  - response-format
  - bash-mcp
  - consistency
plan_ref: "mcp-bash-unified-metadata"
---

## Problem

Response metadata fields were inconsistently ordered and partially missing across the bash MCP server's four execution tools: `exec`, `spawn`, `wait`, and `terminate`. Some tools emitted `status:`, others didn't. Field ordering varied by call site, making programmatic parsing fragile and user-facing output inconsistent.

## Symptoms

- `exec` success response: missing `status:` and `command:` fields
- `spawn` response: wrong field order (execution_id, stdout_log_path, stderr_log_path, working_dir, command)
- `terminate` response: missing `working_dir:` field; field order undefined
- `render_timeout_message` (exec timeout): missing `status:` and `command:` fields
- No canonical order invariant — each call site duplicated field emission with subtle variations

## Investigation Steps

1. Surveyed all 7 call sites emitting metadata: exec success, exec timeout, spawn, wait-exit, wait-running, terminate-unix, terminate-windows
2. Constructed a field presence matrix per tool (from plan notes) showing gaps
3. Identified canonical field order: `execution_id`, `status`, `exit_code`, `command`, `working_dir`, `stdout_log_path`, `stderr_log_path`, `total_lines`, `total_bytes`
4. Traced `TimeoutRenderContext` struct to confirm it lacked `command` field
5. Designed `MetadataHeader<'a>` struct with all Option fields to allow selective emission per tool

## Root Cause

Each tool response built metadata fields inline with manual `writeln!` calls. No shared helper existed to enforce order. Fields were added incrementally without cross-tool consistency checks, leading to divergent output formats across `exec`, `spawn`, `wait`, `terminate`.

## Solution

Introduced `MetadataHeader<'a>` struct and `render_metadata_header()` helper function:

```rust
struct MetadataHeader<'a> {
    execution_id: Option<&'a str>,
    status: Option<&'a str>,
    exit_code: Option<i32>,
    command: Option<&'a str>,
    working_dir: Option<&'a Path>,
    stdout_log_path: Option<&'a Path>,
    stderr_log_path: Option<&'a Path>,
    total_lines: Option<usize>,
    total_bytes: Option<usize>,
}

fn render_metadata_header(output: &mut String, metadata: MetadataHeader<'_>) {
    let MetadataHeader {
        execution_id, status, exit_code, command, working_dir,
        stdout_log_path, stderr_log_path, total_lines, total_bytes,
    } = metadata;

    if let Some(execution_id) = execution_id {
        let _ = writeln!(output, "execution_id: {execution_id}");
    }
    if let Some(status) = status {
        let _ = writeln!(output, "status: {status}");
    }
    // ... remaining fields in canonical order
}
```

**Call site updates:**

- `exec` success: added `status: exited`, `command:`
- `render_timeout_message`: added `status: timeout`, `command:`; extended `TimeoutRenderContext` with `command: &'a str`
- `spawn`: added `status: spawned`; reordered fields
- `terminate` (unix/windows): added `status: terminated`, `working_dir:`

**Status values:**

- `exited` — process completed
- `running` — wait returned while process still alive
- `spawned` — background process started
- `terminated` — signal sent to process
- `timeout` — exec timed out

## Why This Works

**Centralized ordering invariant** — Single helper function guarantees canonical field order across all tools. Adding new fields requires one location change.

**Option<&'a _> fields** — None values silently omitted. Each call site explicitly populates relevant fields without conditional branches.

**Lifetime-annotated struct** — Avoids allocations; borrows string slices from caller context.

## Prevention Strategies

**Test cases:**

- `test_exec_basic_command`: position assertions verifying `execution_id < status < exit_code < command < working_dir < stdout_log_path < stderr_log_path`
- `test_exec_timeout`: verify `status: timeout` and `command:` present
- `test_spawn_and_wait`: verify `status: spawned` in spawn response
- `test_spawn_and_terminate`: verify `status: terminated` and `working_dir:` in terminate response

**Code review checklist:**

- [ ] All new tool responses use `render_metadata_header()`
- [ ] MetadataHeader fields populated explicitly (no defaults)
- [ ] Status value matches tool context (exited/running/spawned/terminated/timeout)

## Related Issues

- **Issue:** #363 — Unified metadata fields
- **Related Solution:** [integration-issues/stable-execution-identifiers-2026-04-27.md](../integration-issues/stable-execution-identifiers-2026-04-27.md) — execution_id generation and log path structure
