---
title: "Sandbox Project-Root Autodetection via Pseudo-Variable Path Syntax"
date: 2026-06-02
category: workflow-issues
problem_type: workflow_issue
component: harnx-sandbox-common
root_cause: "Hardcoded sandbox paths did not adapt to project context; linked git worktrees lacked access to primary .git; HOME guard missing from harnx-mcp-bash extra paths"
resolution_type: code_fix
severity: medium
tags:
  - sandboxing
  - path-resolution
  - git-worktree
  - cross-platform
  - testing
  - security-invariant
plan_ref: sandbox-project-root-detection
---
  
## Problem

Sandboxing development tools (yarn, cargo, go) required manually specifying project paths. Users in git linked worktrees could not access the primary `.git` data directory (history, hooks). `harnx-mcp-bash` did not apply the `is_home_or_ancestor` security guard to `--extra-*` paths, allowing accidental HOME exposure.

## Symptoms

- `harnx-sandbox-run --extra-rwx '$GIT_ROOT' -- cargo build` failed or required manual path calculation
- Linked worktree users: git history commands failed inside sandbox (no access to primary `.git`)
- `harnx-mcp-bash`: `HARNX_BASH_EXTRA_RWX=$HOME` granted full home access (defect)
- Monorepos: user had to determine correct workspace root manually

## Investigation Steps

1. Identified that `harnx-sandbox-run` and `harnx-mcp-bash` each had their own `expand_tilde` implementations — code duplication
2. Traced `harnx-mcp-bash` extra path handling: no `is_home_or_ancestor` check (defect from tilde-expansion PR)
3. Evaluated git library options: `gix` already a workspace dep (used by `harnx-mcp-history`), no subprocess
4. Tested `gix::Repository::common_dir()` for linked worktrees: returns non-normalized path (`.git/worktrees/<name>/../..`), but canonicalizes correctly through `is_home_or_ancestor`
5. Designed marker-fs walk-up: Node/Cargo = highest ancestor (monorepo hoisted deps), Go = nearest (independent modules)
6. Hit flaky test failures under `cargo test`: env-mutating tests across modules in single binary used independent `OnceLock<Mutex>` locks — does NOT serialize. Single `OnceLock<Mutex<()>>` per *binary* required

## Root Cause

**No shared path resolution**: Each binary had its own `expand_tilde`, no project-root detection. Users hardcoded paths or wrote wrapper scripts.

**Missing HOME guard**: `harnx-mcp-bash` extra paths bypassed `is_home_or_ancestor` check introduced for security incident #619.

**Test isolation bug**: `OnceLock<Mutex<()>>` declared per-module. Under `cargo test` (single process, threads), per-module locks do NOT serialize against each other. Only `nextest` masks this (process isolation per test).

## Solution

Added project-root pseudo-variables to sandbox path flags, shared in `harnx-sandbox-common`.

### New Modules

**`root_detection.rs`** — detects project roots via `gix` and marker-fs walk-up:
```rust
pub enum RootKind {
    GitRoot,         // gix::discover(cwd).workdir()
    GitCommonDir,    // gix::discover(cwd).common_dir()
    NodeProjectRoot, // walk up for package.json (highest)
    CargoRoot,       // walk up for Cargo.toml (highest)
    GoRoot,          // walk up for go.mod (nearest)
}

pub fn detect_project_root(kind: RootKind, cwd: &Path) -> Option<PathBuf> {
    let root = match kind { /* ... */ };
    if crate::is_home_or_ancestor(&root) {
        return None;  // Security invariant: never allow HOME/ancestor
    }
    Some(root)
}
```

**`path_expand.rs`** — shared path expansion:
```rust
#[cfg(unix)]
pub fn expand_path_var(raw: &str, cwd: &Path) -> Option<PathBuf> {
    // Match pseudo-var at exact prefix-boundary only
    let pseudo_var = [
        ("$GIT_ROOT", RootKind::GitRoot),
        ("$GIT_COMMON_DIR", RootKind::GitCommonDir),
        // ...
    ].into_iter().find(|(prefix, _)| {
        raw == *prefix || raw.strip_prefix(prefix).is_some_and(|r| r.starts_with('/'))
    });
    
    if let Some((prefix, kind)) = pseudo_var {
        let root = detect_project_root(kind, cwd)?;
        let remainder = raw.strip_prefix(prefix).expect("matched");
        return Some(match remainder.strip_prefix('/') {
            Some(relative) => root.join(relative),
            None => root,
        });
    }
    
    Some(PathBuf::from(expand_tilde(raw)))
}

#[cfg(not(unix))]
pub fn expand_path_var(raw: &str, _cwd: &Path) -> Option<PathBuf> {
    Some(PathBuf::from(raw))  // consume-and-ignore
}
```

**`test_support.rs`** — single process-wide env lock per crate:
```rust
pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    // Recover from poisoning: a panicking test still leaves the lock usable for
    // the next one, which only needs serialized access (not invariant integrity).
    match LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

pub(crate) struct EnvGuard {
    saved_home: Option<OsString>,
    saved_cwd: PathBuf,
}
// RAII: restores HOME/cwd on drop
```

### Home Guard Fix

Added `is_home_or_ancestor` check to `harnx-mcp-bash` extra paths in `args.rs`:
```rust
for path in extra_paths {
    #[cfg(unix)]
    if is_home_or_ancestor(&path) {
        eprintln!("harnx-mcp-bash: dropping --extra-* path resolving to $HOME or ancestor");
        continue;
    }
    // ... add to sandbox args
}
```

### Test Isolation Fix

Consolidated `OnceLock<Mutex<()>>` into single `test_support.rs` per crate. All test modules in that crate import from same location:
- `harnx-sandbox-common/src/test_support.rs` — used by `home_guard`, `root_detection`, `path_expand`, `args` test modules
- `harnx-mcp-bash/src/test_support.rs` — used by `main.rs` tests and `server/tests.rs`

## Why This Works

**Security invariant reused**: `is_home_or_ancestor` from prior incident (#619) guards every detected root. Canonicalizes both HOME and candidate, checks `home.starts_with(&candidate)`. Symlink-to-HOME resolved via canonicalization. Child of `$HOME` allowed; `$HOME` or ancestor blocked.

**`common_dir()` handles linked worktrees**: `gix::Repository::common_dir()` returns primary `.git` path (even from linked worktree). Returns non-normalized path with `../..` segments, but `is_home_or_ancestor` canonicalizes internally, defeating this gotcha. `workdir()` returns linked worktree root.

**Highest vs Nearest semantics**: Node/Cargo walk to highest ancestor with marker — enables monorepo workspace root (shared `node_modules`, shared `target/`). Go walks to nearest `go.mod` — nested modules are independent.

**Prefix-boundary matching**: `$GIT_ROOT` matches, `$GIT_ROOTX` literal (prefix not followed by `/` or end). Remainder joined relative: strip leading `/` so `join()` appends to root.

**`Option<PathBuf>` return**: Silent-skip-on-miss for pseudo-vars (no git repo → no path). Literal/tilde always `Some`.

**Cross-platform**: `#[cfg(unix)]` detection; non-Unix `expand_path_var` returns raw path unchanged. Symbol exists on both, compiles.

**Single env lock per binary**: `OnceLock<Mutex<()>>` is process-global. Multiple declarations in different modules = multiple locks = does NOT serialize. Single `test_support.rs` with `pub(crate)` lock used by all modules in same binary. `cargo test` runs tests in single process; `nextest` isolates per test.

## Prevention Strategies

**Test Cases:**
- Pseudo-var expands in git repo, drops outside repo
- `$GIT_COMMON_DIR` resolves to primary `.git` from linked worktree (real `git worktree add` test)
- Symlink pointing to `$HOME` dropped by guard
- `$GIT_ROOTX` literal, `/foo/$VAR` literal (no expansion mid-path)
- HOME/cwd mutation tests serialize on shared env lock

**Best Practices:**
- Every path added to sandbox MUST pass `is_home_or_ancestor`
- Adding new pseudo-var? Update array in `path_expand.rs`, add `RootKind` variant
- Marker-fs detection: mono-root tools (Node, Cargo) = highest; independent modules (Go) = nearest
- `gix` paths may be non-normalized; canonicalize before string comparison

**Code Review Checklist:**
- [ ] New sandbox path sources apply `is_home_or_ancestor`?
- [ ] Path expansion called at both CLI and env-var entry points?
- [ ] Non-Unix compile passes (consume-and-ignore)?
- [ ] env-mutating tests use SINGLE shared `OnceLock<Mutex<()>>` per crate?

**Test Isolation Rule:**
`OnceLock<Mutex<()>>` must be declared exactly ONCE per test binary. If crate has no lib.rs (bin with `mod server`), inline tests and `server/tests.rs` share one binary. Put lock in `test_support.rs`, use `pub(crate)`.

## Related Issues

- **GitHub Issue:** [#575 — Sandbox project-root autodetection via pseudo-variable path syntax](https://github.com/dobesv/harnx/issues/575)
- **Plan:** sandbox-project-root-detection
- **Commits:** a5e2c22b1e, 99c0d39370, 832d688829, 985c0f2090, b827d23178, 358b02f7ff, 6b7f0141b4
- **Related Solution:** [../security-issues/sandbox-home-exposure-ancestor-walk-2026-05-21.md](../security-issues/sandbox-home-exposure-ancestor-walk-2026-05-21.md) — HOME guard invariant (reused)
- **Related Solution:** [sandbox-path-tilde-expansion-cross-platform-2026-04-29.md](sandbox-path-tilde-expansion-cross-platform-2026-04-29.md) — Tilde expansion, env lock pattern
