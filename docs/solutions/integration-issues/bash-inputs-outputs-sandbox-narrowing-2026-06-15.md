---
title: "Fix: Cargo build failures in sub-agents due to bash sandbox narrowing"
date: 2026-06-15
category: integration-issues
problem_type: bug
component: harnx-mcp-bash
root_cause: "Per-call 'outputs' parameter narrowed project roots to exec-only, breaking cargo crate detection"
resolution_type: code_fix
severity: high
tags:
  - sandbox
  - birdcage
  - bash
  - cargo
  - mcp
plan_ref: harnx-850-bash-inputs-outputs
---

## Problem

Sub-agents attempting to run `cargo` commands (like `cargo build` or `cargo test`) frequently failed with `error[E0463]: can't find crate` even when the dependencies were present. This occurred because the `bash_exec` tool's `outputs` parameter was used to narrow filesystem access, but the narrowing logic was too restrictive for Rust's compilation model.

## Root Cause

The `harnx-mcp-bash` tools (`bash_exec`/`bash_spawn`) previously allowed agents to specify `inputs` and `outputs` paths. If provided, these paths narrowed the sandbox's access to the project roots.

When `outputs` were specified:
1. The listed paths were granted read+write+exec access.
2. The project roots (which are normally read+write+exec) were downgraded to **exec-only** if they were not explicitly listed in `outputs`.

This broke `cargo` because it needs to read metadata and lockfiles from the project root even when it is only writing to `target/`. When the project root became exec-only, `cargo` could no longer "see" the crate structure or its dependencies, leading to "can't find crate" errors.

## Solution

The `inputs` and `outputs` parameters were removed from the `bash_exec` and `bash_spawn` tool definitions.

- **Unconditional Root Access:** Project roots now always receive read+write+exec access in the sandbox.
- **Removed Narrowing:** The per-call narrowing logic was deleted.
- **Legacy Compatibility:** The tools still accept `inputs` and `outputs` arguments from legacy callers to avoid breaking existing agents, but these arguments are ignored.
- **History Snapshots:** Command classification is now used exclusively to determine when to take history snapshots, rather than relying on the presence of `outputs`.

## Symptoms Before Fix

```text
error[E0463]: can't find crate for `harnx_core`
  --> src/main.rs:1:1
   |
 1 | extern crate harnx_core;
   | ^^^^^^^^^^^^^^^^^^^^^^^^ can't find crate
```

## Prevention Strategies

- **Avoid Per-Call Narrowing of Primary Roots:** Primary project roots should maintain consistent permissions to ensure toolchain stability.
- **Use Command Classification for Side Effects:** Use AST or pattern-based command classification to detect mutations rather than relying on agent-supplied path hints.
- **Verify Toolchain Requirements:** When implementing sandboxing, verify that common toolchains (Cargo, NPM, Go) can still access necessary metadata files in the project structure.

## Related Issues

- **GitHub Issue:** [#850](https://github.com/dobesv/harnx/issues/850)
- **Plan:** harnx-850-bash-inputs-outputs
- **Supersedes:** [cli-wrapper-sandboxing-for-tokio-servers-2026-04-28.md](./cli-wrapper-sandboxing-for-tokio-servers-2026-04-28.md) (in part)
