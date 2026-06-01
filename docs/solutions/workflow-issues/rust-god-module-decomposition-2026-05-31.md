---
title: "Decomposing Rust god-modules into cohesive submodules for CodeScene hotspot remediation"
date: 2026-05-31
category: "workflow-issues"
problem_type: workflow_issue
component: "harnx-mcp-bash, harnx-mcp-plans, harnx-mcp-fs"
root_cause: "Monolithic server files exceeding 1500-5000 lines with Low Cohesion scores 2.8-4.8"
resolution_type: code_fix
severity: medium
tags:
  - rust
  - refactoring
  - codescene
  - module-split
  - cohesion
  - god-module
plan_ref: "cs-code-health-cleanup"
---

## Problem

Three MCP server crates contained monolithic `server.rs` files (1500-5242 lines) flagged by CodeScene as hotspots with Low Cohesion scores of 2.8-4.8 and 12+ responsibilities per file. These "god-modules" mixed parameter definitions, handler logic, storage operations, and tests, making navigation difficult and increasing cognitive load.

## Symptoms

```
- server.rs: 1549-5242 lines per file
- CodeScene Low Cohesion: 2.8-4.8 (threshold ≥7.0)
- CodeScene findings: "Low Cohesion", "Large Module", "High Method Count"
- Navigation friction: finding logic required scrolling hundreds of lines
- Merge conflict probability increased with multiple contributors
```

## Investigation Steps

Started with CodeScene hotspot report identifying 99 imperfect files. Three large server modules prioritized due to severity:

1. `harnx-mcp-bash/src/server.rs` — 5242 lines, score 2.81
2. `harnx-mcp-plans/src/server.rs` — 4287 lines, score 4.85
3. `harnx-mcp-fs/src/server.rs` — 2980 lines, score 3.6

Initial sub-agent delegation for module splits failed reliably due to:
- Brace-level extraction errors (over-removing contiguous ranges)
- Inability to create new files (read-only FS for new paths)
- Batch edits leaving tree non-compiling with 70+ errors

Rolled back and developed orchestrator-driven approach with deterministic Python brace-matching extraction.

## Root Cause

**File-level vs method-level findings distinction**: File-level findings (Low Cohesion, Large Module) require module splitting. Method-level findings (High Complexity, Long Method) require in-place refactoring. Matching remediation to finding type was essential.

The god-modules accumulated because:
- Early development favored single-file simplicity
- MCP handler logic grew organically without architectural boundaries
- No enforced module size limits in CI

## Solution

### orchestrator-driven Python extraction workflow

```
For each god-module:
1. cs review → identify file-level vs method-level findings
2. Group methods into cohesive clusters (shared fields, call graph, naming)
3. Orchestrator creates empty placeholder files via fs_write
4. Python brace-matching extractor moves methods into split files
5. cargo check after each extraction
6. Fix cross-module visibility
7. Re-run cs on each resulting file; iterate until ≥7.0
```

### Module structure after decomposition

**harnx-mcp-bash/server/** (from 5242 lines, score 2.81 → all files ≥8.0):
```
mod.rs         — imports, struct definitions, module wiring
params.rs      — request parameter structs + JsonSchema impls
handler.rs     — ServerHandler trait impl (routing layer)
handlers.rs    — context structs (SandboxAcc, *Ctx types)
lifecycle.rs   — new, refresh_roots, cleanup
command.rs     — sandbox args, run_to_completion
exec.rs        — exec_command_impl, rollback_file_impl
process.rs     — spawn_impl, wait_impl, terminate_impl
exec_log.rs    — read_exec_log_impl, pagination
env.rs         — environment loading, bashrc parsing
render.rs      — output formatting, metadata headers
sandbox.rs     — path guards (is_home_or_ancestor)
tests.rs       — test module (file-module style)
```

**harnx-mcp-plans/server/** (from 4287 lines, score 4.85 → all files ≥7.0):
```
mod.rs         — imports, PlansServer struct
params.rs      — all param/frontmatter structs + impl_json_schema! macro
handler.rs     — ServerHandler trait impl
handlers.rs    — 15 handle_* methods
store.rs       — filesystem ops, cleanup_loop
tests.rs       — comprehensive test suite
```

**harnx-mcp-fs/server/** (from 2980 lines, score 3.6 → all files ≥7.0):
```
mod.rs         — imports, FsServer struct
params.rs      — 9 param structs + JsonSchema impls
handler.rs     — ServerHandler trait impl
handlers.rs    — *_impl tool methods
walk.rs        — directory traversal, grep logic
tests.rs       — test module
```

### Dependency hygiene pattern

```rust
// In each submodule (e.g., handlers.rs):
use super::*;  // Import from parent mod.rs — always correct direction

// In mod.rs:
pub(crate) use params::*;
pub(crate) use store::*;
pub use store::cleanup_loop;  // For external consumers (main.rs)
```

Key rules:
- Submodules use `use super::*;` — no child-to-child imports
- `pub(crate)` for cross-module internal access
- `pub` for external API (e.g., cleanup_loop re-export)
- No circular dependencies possible with unidirectional imports

### Visibility strategy

| Symbol Type | Visibility | Rationale |
|-------------|------------|-----------|
| Public API structs | `pub` | External consumers need access |
| Handler methods | `pub(crate)` | Called from handler.rs routing |
| Internal helpers | private | Implementation detail |
| Param structs | `pub(crate)` | Used across submodules |

### Behavioral equivalence verification

```
✓ cargo build -p <crate>
✓ cargo clippy -p <crate> --all-targets -- -D warnings
✓ cargo nextest run --workspace  (1279 passed, 1 skipped)
✓ cargo fmt --check
```

Preserved byte-identical:
- Error message strings
- Snapshot labels
- cfg gates
- Diff path formats

## Why This Works

**Unidirectional imports**: All submodules import from parent via `use super::*`. This creates a clean dependency tree rooted at mod.rs with no cycles possible.

**Cohesion-first clustering**: Grouping by shared state and call graph ensures each resulting module has a single, clear purpose. Files like `exec_log.rs` or `process.rs` are self-contained.

**Orchestrator control**: Sub-agents cannot reliably perform file creation or brace-level extraction. The orchestrator pre-creates placeholders and runs deterministic extraction, preventing compilation drift.

**Incremental verification**: Running `cargo check` after each extraction catches visibility and import errors immediately, before compound failures accumulate.

## Prevention Strategies

**Test cases that would catch cohesion regressions**:
- Code CI check: `cs review --fail-under 7.0` as quality gate
- File size limit: reject files over 800 lines in CI
- Module count limit: flag single-file directories over 1000 lines

**Best practices**:
- Start new MCP servers with `server/mod.rs` structure from day one
- Group handlers by domain (e.g., `plans.rs`, `tasks.rs`, `notes.rs`)
- Extract context structs early to reduce parameter list length

**Code review checklist**:
- [ ] All submodules use `use super::*;` (no sibling imports)
- [ ] Handler methods are `pub(crate)`
- [ ] Re-exports match `main.rs` import paths
- [ ] No unused imports after moves (clippy catches)
- [ ] Each submodule has single cohesive purpose

## Gotchas

1. **Module naming collision**: Never name submodules after std modules (e.g., `log.rs` collides with `log::warn!`). Use `exec_log.rs` instead.

2. **glob re-exports don't upgrade visibility**: `pub(crate) use store::*` makes items `pub(crate)`, not `pub`. For external consumers, add explicit `pub use store::cleanup_loop`.

3. **Sub-agents cannot create files**: Pantheon sub-agents have read-only FS for new paths. Orchestrator must pre-create placeholder files.

4. **Test files have different economics**: `tests.rs` files with Low Cohesion from test count are lower priority (test code, not production). Defer decomposition.

5. **Unused imports after moves**: Moving functions leaves imports behind. Run `cargo clippy -D warnings` to catch.

## Related Issues

- **Prior solution**: [integration-issues/stable-execution-identifiers-2026-04-27.md](../integration-issues/stable-execution-identifiers-2026-04-27.md) — Initial CodeScene advisory about server.rs growth
- **Plan**: cs-code-health-cleanup — Full campaign context and review notes
