---
title: "Sandbox $HOME Exposure via Write-Path Ancestor Walk and Over-Broad Roots"
date: 2026-05-21
category: security-issues
problem_type: security_issue
component: harnx-mcp-bash
root_cause: "Non-existent write paths triggered ancestor walk to $HOME; CWD prepended as root without HOME boundary check"
resolution_type: code_fix
severity: high
tags:
  - sandboxing
  - birdcage
  - privilege-escalation
  - home-directory
  - path-traversal
  - defense-in-depth
plan_ref: sandbox-home-exposure-fix
---

## Problem

Two security vulnerabilities in the birdcage sandbox implementation allowed `$HOME` directory exposure when:

1. A `--write` path didn't exist, triggering an ancestor-walk loop that granted `WriteAndRead` to the first existing ancestor — potentially `$HOME` if subpaths like `~/.pyenv` or `~/.rye` were missing
2. The current working directory (CWD) equaled or was an ancestor of `$HOME` and got prepended as an MCP root, receiving sandbox permissions via `build_sandbox_args`

## Symptoms

**Write-path ancestor walk (#619):**
- When `HOME_RWX_PATHS` included `~/.pyenv` or `~/.rye` and those directories didn't exist, sandbox-run walked up to `$HOME` and mounted it read-write
- `~/.aws`, `~/.ssh`, and other sensitive directories became accessible to sandboxed child processes
- Non-deterministic: depended on whether tool-specific directories existed on the host

**Over-broad CWD roots (#503):**
- Agent started in `$HOME` or `/home` received write/exec permissions on entire home directory
- Roots arriving via MCP peer `refresh_roots` could bypass single-point validation
- Silent permission grant — no error or warning when over-broad root was added

## Investigation Steps

1. **Reproduced #619**: Created test with non-existent `~/.harnx-test-pyenv-NOTEXIST` and verified ancestor walk reached `$HOME`
2. **Traced loop**: Found `add_write_exception` used `loop { path = path.parent(); ... }` pattern identical to earlier `add_path_exception` bug
3. **Analyzed #503**: Reviewed `reinit_managers_for_agent` prepending CWD to `extra_roots` without boundary checks
4. **Discovered second injection point**: `build_sandbox_args` receives roots from multiple sources (CWD, MCP peer) — single guard insufficient
5. **Reviewed canonicalization edge cases**: Non-existent paths fail `canonicalize` — needed fallback to raw path comparison

## Root Cause

**Fix #619**: `add_write_exception` in `sandbox_run.rs` used a parent-walk loop to handle non-existent paths. When a `--write` path didn't exist, it walked ancestors until finding an existing directory, then granted `WriteAndRead` to that ancestor. If `HOME_RWX_PATHS` like `~/.pyenv` were missing, the walk reached `$HOME`.

**Fix #503**: `reinit_managers_for_agent` unconditionally prepended CWD to MCP server roots. If CWD was `$HOME` or an ancestor like `/home`, that root received `--write`/`--exec` permissions. Roots also arrived via `refresh_roots` from MCP peers, requiring defense at sandbox construction time.

## Solution

### Fix #619: Remove ancestor walk, skip non-existent paths

Changed `add_write_exception` to match `add_path_exception` behavior:

```rust
// BEFORE: Ancestor walk could reach $HOME
fn add_write_exception(sandbox: &mut Birdcage, path: &Path) -> Result<(), String> {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return sandbox
                .add_exception(Exception::WriteAndRead(current))
                .map_err(|e| format!("Failed to add write exception: {}", e));
        }
        current = match current.parent() {
            Some(p) => p.to_path_buf(),
            None => return Err(format!("No existing ancestor for: {}", path.display())),
        };
    }
}

// AFTER: Skip non-existent paths entirely
fn add_write_exception(sandbox: &mut Birdcage, path: &Path) -> Result<(), String> {
    if !path.exists() {
        eprintln!("sandbox-run: skipping non-existent path: {}", path.display());
        return Ok(());
    }
    sandbox
        .add_exception(Exception::WriteAndRead(path.to_path_buf()))
        .map_err(|e| format!("Failed to add write exception: {}", e))
}
```

### Fix #503: Defense in depth at injection and construction points

**Helper function:** Detect `$HOME` and ancestors using `starts_with`:

```rust
#[cfg(unix)]
fn is_home_or_ancestor(path: &Path) -> bool {
    use std::env::var_os;
    use std::path::PathBuf;

    let home = match var_os("HOME") {
        Some(h) => PathBuf::from(h),
        None => return false,  // No HOME = no boundary to enforce
    };

    let home_canonical = home.canonicalize().unwrap_or_else(|_| home.clone());
    let path_canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    home_canonical.starts_with(&path_canonical)
}
```

Key insight: `home.starts_with(candidate)` returns `true` when:
- `candidate` == `$HOME` (path IS home)
- `candidate` is an ancestor like `/home` or `/` (home starts with ancestor)

**Guard 1: Prevent over-broad roots at injection**

```rust
// In reinit_managers_for_agent (config/mod.rs)
#[cfg(unix)]
{
    if is_home_or_ancestor_of_home(&cwd) {
        warn!(
            "sandbox: skipping CWD {:?} as MCP root — equals or is ancestor of $HOME",
            cwd.display()
        );
    } else {
        roots.push(cwd);
    }
}
```

**Guard 2: Filter roots at sandbox construction**

```rust
// In build_sandbox_args (server.rs)
for root in roots.iter() {
    #[cfg(unix)]
    if is_home_or_ancestor(root) {
        continue;  // Block HOME and ancestors
    }
    args.push(OsString::from("--write"));
    args.push(root.as_os_str().to_os_string());
}
```

## Why This Works

1. **Skip-not-walk for #619**: Removing the loop eliminates the privilege escalation path entirely. Non-existent paths are simply skipped — no ancestor is ever granted access. This matches the safer behavior of `add_path_exception`.

2. **Defense in depth for #503**: Guarding at both injection (`reinit_managers_for_agent`) AND construction (`build_sandbox_args`) handles all root sources:
   - CWD prepended at initialization → blocked by Guard 1
   - Roots from MCP peer `refresh_roots` → blocked by Guard 2

3. **Canonicalization with fallback**: `is_home_or_ancestor` uses `canonicalize().unwrap_or_else(|_| raw_path)` to handle:
   - Non-existent paths (canonicalize fails, use raw)
   - Symlinks (canonicalize resolves to real path)
   - Both HOME and candidate failing canonicalize (raw comparison)

4. **Unix-only guards**: HOME concept is Unix-specific. Windows builds skip guards entirely — no behavior change. `#[cfg(unix)]` applied consistently.

## Prevention Strategies

**Test Cases:**
- Regression test for #619: assert non-existent nested path doesn't trigger ancestor walk
- Boundary tests: HOME itself blocked, `/home` ancestor blocked, `$HOME/projects` child allowed
- CWD tests: `CWD == $HOME` blocked, `CWD == /home` blocked, `CWD == $HOME/projects` allowed
- Env lock serialization: use `OnceLock<Mutex<()>>` to prevent HOME race conditions in tests

```rust
fn env_lock() -> MutexGuard<'static, ()> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}
```

**Best Practices:**
- Never walk ancestors to find "closest" existing path for security boundaries
- Multi-source roots require defense in depth — guard at all injection points AND at consumption
- Canonicalize paths before boundary checks, but handle failures gracefully
- Test both the fix AND that the original bug would have been caught

**Code Review Checklist:**
- [ ] Do path exception functions skip non-existent paths, not walk ancestors?
- [ ] Are HOME boundary guards at both injection and construction points?
- [ ] Is canonicalization fallback handling non-existent paths correctly?
- [ ] Are guards Unix-only where HOME concept applies?
- [ ] Do tests verify sandbox state, not just function return values?

**Note on test strength:** The regression test `test_write_exception_nonexistent_nested_no_ancestor_walk` only asserts `result.is_ok()`. Since the buggy implementation also returned `Ok(())` (after adding $HOME as exception), this test passes on broken code. Stronger tests should verify actual sandbox state or end-to-end filesystem access.

## Related Issues

- **GitHub Issue:** [#619 — Sandbox bypass via --write ancestor walk](https://github.com/dobesv/harnx/issues/619)
- **GitHub Issue:** [#503 — Avoid giving access to home directory automatically](https://github.com/dobesv/harnx/issues/503)
- **Plan:** sandbox-home-exposure-fix
- **Commit:** 3ff427b — fix(sandbox): prevent $HOME exposure via write-path ancestor walk and over-broad roots
- **Related Solution:** [environment-sanitization-bash-sandbox-2026-04-29.md](environment-sanitization-bash-sandbox-2026-04-29.md) — Environment variable sanitization for sandboxed child processes
- **Related Solution:** [../workflow-issues/sandbox-path-tilde-expansion-cross-platform-2026-04-29.md](../workflow-issues/sandbox-path-tilde-expansion-cross-platform-2026-04-29.md) — Sandbox path configuration and exec permissions
