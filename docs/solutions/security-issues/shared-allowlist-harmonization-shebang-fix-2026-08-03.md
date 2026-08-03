---
title: "Shared Allowlist Architecture: Harmonized fs/bash Sandbox + Shebang Security Fix"
date: 2026-08-03
category: security-issues
problem_type: security_issue
component: "harnx-tool-allow/harnx-fs-tools/harnx-bash-tools"
root_cause: "Per-tool ad-hoc allowlists caused cross-tool confusion; latent shebang exec-grant sandbox-escape activated by arg-refactor"
resolution_type: code_fix
severity: high
tags:
  - sandboxing
  - allowlist
  - shebang
  - security-boundary
  - cross-tool-consistency
  - home-guard
  - deny-all-by-default
plan_ref: "tool-allow-whitelist-harmonization"
---

## Problem

fs and bash MCP servers used independent allowlist implementations (fs: flat roots Vec with no r/w/x distinction; bash: implicit system defaults + `--extra-*` flags + project-root detection). Same inputs produced different accessible sets — agents received "outside allowed roots" from fs for paths bash had just used. Additionally, bash's dynamic shebang-interpreter exec-grant was a latent sandbox escape that a naive arg-rewrite could have activated.

## Symptoms

```
- Error: "outside allowed roots" from fs for paths bash could access
- Behavior: Agent confusion when cross-tool path expectations diverged
- Latent vuln: shebang scripts with `#/opt/notallowed/x` would auto-grant exec on /opt/notallowed
- Regression risk: Arg-refactor could enable the shebang escape path
```

## Investigation Steps

Traced the divergence: `harnx-mcp::safety` exported `validate_path`/`validate_write_path` called by both tools, but their allowlist inputs came from different sources. Bash combined implicit defaults (` SYSTEM_EXEC_PATHS`, `/tmp`, `~/.cargo`) with explicit `--extra-*`. Fs had only explicit roots via MCP protocol. Neither enforced write/exec ⊇ read closure consistently.

Found the shebang issue in `command.rs`: when a script had an absolute shebang interpreter (`#/opt/notallowed/x`), bash would auto-grant `--exec` on the interpreter's parent directory. This grant bypassed validation because it happened during sandbox-arg construction.

Confirmed the ordering constraint: P3a (shebang security fix) HAD to land before P3b (bash sandbox-arg refactor). The refactor would touch the same code paths and could have activated the latent vuln.

Discovered the history-manager hang during P3b testing: `HistoryManager::new(writable_paths)` performed eager recursive git-repo discovery. With `--allow-common-default` granting `/tmp` and `/dev/shm`, startup blocked indefinitely.

## Root Cause

**Allowlist divergence:** Each tool interpreted "roots" differently. fs treated roots as read+write (no exec concept). bash treated roots as rwx plus implicit system paths. Neither enforced the $HOME-ancestor guard consistently across write/exec grants.

**Shebang exec-grant escape:** The dynamic grant in `command.rs` added `--exec` for shebang interpreter directories without checking against the resolved allowlist. This gave sandboxed processes execute access to arbitrary directories.

**History-manager eager scan:** `BashServer::new_with_sandbox` passed all writable paths to `HistoryManager::new`, triggering unbounded filesystem walks. Broad batches like `common_default` made this O(unbounded).

## Solution

### 1. Shared Allowlist Resolver Crate

Created `harnx-tool-allow` crate with `ResolvedAllowlist { read, write, exec }` (BTreeSet<PathBuf> each):

```rust
pub struct ResolvedAllowlist {
    read: BTreeSet<PathBuf>,
    write: BTreeSet<PathBuf>,
    exec: BTreeSet<PathBuf>,
}

impl ResolvedAllowlist {
    pub fn insert_write(&mut self, path: PathBuf) {
        self.write.insert(path.clone());
        self.read.insert(path);  // write ⊇ read closure
    }
}
```

Both `harnx-fs-tools` and `harnx-bash-tools` now call `resolve_allowlist(&AllowInputs, cwd, &AllowEnv)` to get identical accessible sets for identical inputs. fs ignores the exec set (no exec operation).

### 2. $HOME-Ancestor Guard Centralized

Every write/exec/rwx insertion goes through `home_guard::is_home_or_ancestor`:

```rust
// In batches.rs
fn push_guarded(rules: &mut Vec<AllowRule>, path: PathBuf, perm: Permission, env: &AllowEnv) {
    if let Some(home) = &env.home {
        if is_home_or_ancestor(&path, home) {
            rules.push((path, Permission::Read));  // downgrade
            return;
        }
    }
    rules.push((path, perm));
}
```

Even `--allow-all` gets downgraded: `$HOME` remains read-only regardless of grant source. Same guard as [sandbox-home-exposure-ancestor-walk](./sandbox-home-exposure-ancestor-walk-2026-05-21.md).

### 3. Shebang Exec-Grant Removed

P3a removed the dynamic grant BEFORE P3b touched sandbox-arg construction:

```diff
- if interp.is_absolute() {
-     if let Some(interp_dir) = interp.parent() {
-         let dir_str = interp_dir.to_string_lossy();
-         if !SYSTEM_EXEC_PATHS.iter().any(|p| *p == dir_str.as_ref()) {
-             sb_args.push(OsString::from("--exec"));
-             sb_args.push(interp_dir.as_os_str().to_owned());
-         }
-     }
- }
  sb_args.push(interp.as_os_str().to_owned());
```

Shebang dispatch (parsing, temp script creation, interpreter arguments) remains — only the auto-grant was removed. If the interpreter path isn't in the resolved exec set, the sandbox denies it.

### 4. Ordering Constraint Enforced

Commits landed in sequence: fc3bb81 (P1 crate) → 90d027d (P3a shebang fix) → 280bc1 (P2 fs) → 2df58 (P3b bash) → e3131 (P4 roots removal) → d8635 (P5 docs). P3a BEFORE P3b was critical — the P3b refactor would have touched the same arg-construction code and could have activated the latent vuln.

### 5. History-Manager Lazy Discovery

Changed `BashServer::new_with_sandbox` to:

```rust
let history = HistoryManager::new(&[]);  // eager → lazy
```

HistoryManager already does lazy discovery via `ensure_repo_for_path` / `ensure_repos_under` when paths are actually accessed. Eager scanning broad permission boundaries was unnecessary and caused hangs.

### 6.deny-All-by-Default + Loud Migration

Empty allowlist = hard deny. No cwd/home fallback. Old flags (`--root`, `--extra-*`, `--mcp-root`) fail with exit 1 ("unknown argument"). Shipped yaml updated in the same PR (fs.yaml + bash.yaml opt into `--allow-common-default`, `--allow-dev-tools`, `--allow-repo-work` batches). Migration guide: `docs/migration-allowlist.md`.

### 7. No Half-Migration

`harnx-mcp::safety` validation fns stayed in place until ALL callers migrated (P2 fs, P3b bash, P4 runtime). Then removed. `rg -l 'validate_path|validate_write_path' crates/` confirmed zero callers before deletion.

## Why This Works

**Cross-tool consistency:** Single resolver crate means same CLI flags / env vars produce identical accessible sets. `--allow-rwx ~/.cache` grants read/write/exec for bash and read/write for fs (no exec operation). Agent confusion eliminated.

**Defense-in-depth:** $HOME guard applied at resolver level (every batch, every explicit grant) rather than per-tool validation. Future batches automatically inherit protection.

**Security boundary ordering:** Removing the latent shebang vuln BEFORE refactoring the code that accidentally neutralized it prevented the escape during transition.

**Fail-closed migration:** Old configs fail loudly rather than silently degrading. Users must consciously opt into the new batch model.

## Prevention Strategies

**Test Cases:**
- Assert fs and bash resolve same `AllowInputs` to same read/write sets
- Assert `$HOME` and ancestors receive only read grants regardless of batch/explicit flags
- Assert shebang scripts don't auto-grant exec on interpreter directories
- Assert empty allowlist denies all operations (no cwd/home fallback)
- Stress test batch resolution must complete in <100ms (detect eager-scan regressions)

**Code Review Checklist:**
- [ ] Are all write/exec grants going through `push_guarded` or equivalent?
- [ ] Does new batch code check `is_home_or_ancestor` before privileged grants?
- [ ] Does sandbox-arg construction pull from `ResolvedAllowlist`, not ad-hoc paths?
- [ ] Is there a path that could grant write/exec without resolver validation?

**Best Practices:**
- Shared resolver crates for cross-cutting security boundaries
- When refactoring security code, remove latent vulns BEFORE touching neutralizing code paths
- Audit eager filesystem walks seeded from grant lists — broad permissions → unbounded scans
- Centralize guards (like $HOME-ancestor check) in one place, consume everywhere
- "Don't half-migrate" — old code stays until last caller migrates, then remove atomically

**Monitoring:**
- Log resolved allowlist size at startup (detect config bombs)
- Alert on startup times >5s for tool servers (hang detection)
- Track cross-tool path-validation failures (consistency monitoring)

## Related Issues

- **Issue:** [#1224](https://github.com/dobesv/harnx/issues/1224) — Harmonize fs/bash allowlists
- **Prior Art:** [sandbox-home-exposure-ancestor-walk-2026-05-21.md](./sandbox-home-exposure-ancestor-walk-2026-05-21.md) — $HOME-ancestor guard origin
- **Prior Art:** [shebang-interpreter-dispatch-2026-05-25.md](../logic-errors/shebang-interpreter-dispatch-2026-05-25.md) — Shebang dispatch (grant removed, dispatch kept)
- **Prior Art:** [nats-fs-bash-bridged-cwd-default-2026-07-30.md](../integration-issues/nats-fs-bash-bridged-cwd-default-2026-07-30.md) — Empty-roots hard-deny behavior
- **Migration Guide:** `docs/migration-allowlist.md`
