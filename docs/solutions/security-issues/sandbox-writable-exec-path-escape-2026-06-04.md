---
title: "Sandbox Escape via Writable Executable Path Directories"
date: 2026-06-04
category: security-issues
problem_type: security_issue
component: harnx-sandbox-common
root_cause: "Default whitelist granted write+exec to on-PATH tool directories, enabling trojan injection"
resolution_type: code_fix
severity: high
tags:
  - sandboxing
  - privilege-escalation
  - path-injection
  - trojan
  - least-privilege
  - birdcage
plan_ref: sandbox-default-writable-hardening
---

## Problem

Sandbox default whitelist granted `read+write+exec` to many `$HOME` tool directories. Writable + executable + on-host-PATH = sandbox escape: a malicious sandboxed agent plants a trojan binary in such a directory; the user later runs that command on the HOST and executes it, bypassing the sandbox entirely.

## Symptoms

- Default sandbox configuration allowed write access to directories like `~/.cargo/bin`, `~/.nvm`, `~/.pyenv`, `~/.local/share/claude`, `~/.local/share/pipx`
- These directories are commonly on `$PATH` or executed by shell initialization scripts
- No explicit mechanism prevented a sandboxed process from modifying executables the host would later run
- Attack vector: sandboxed agent writes malicious binary → user runs `cargo`/`node`/`python` on host → trojan executes with full user privileges

## Investigation Steps

Analyzed `crates/harnx-sandbox-common/src/defaults.rs` constants:

- `HOME_RWX_PATHS` contained 15 paths with blanket `read+write+exec`
- `HOME_EXEC_PATHS` and `HOME_WRITE_PATHS` had partial overlap
- Checked which directories are on `$PATH` or commonly executed:
  - `.nvm`, `.cargo/bin`, `.pyenv`, `.rye`, `.local/share/{claude,opencode,pipx}` — all on `$PATH` or shell init
  - `.npm`, `.yarn`, `.cargo/registry`, `.cargo/git`, `.local/share/{pnpm,uv}` — pure caches, not on `$PATH`

Verified two consumers of these constants:
1. `defaults.rs::push_home_relative_defaults` → used by `args.rs` (bash MCP)
2. `sandbox-run/src/sandbox.rs` lines 76-107 → own loops reading same constants

Found divergence: `sandbox.rs` WRITE loop emitted only `--write` flag, while `args.rs` emitted `--read+--write`. Both paths map to `Exception::WriteAndRead` in `sandbox_exec.rs`, so divergence was cosmetic but required alignment for consistency.

## Root Cause

Security model violated the principle of least privilege. Default configuration prioritized convenience over security by granting `write+exec` to directories whose contents the host would execute. This created a privilege escalation path:

```
Sandboxed process writes trojan → ~/.cargo/bin/my_tool
User runs my_tool on host → trojan executes outside sandbox
```

Threat model: compromised dependencies, prompt injection, or malicious agent code could plant backdoors that persist beyond sandbox lifecycle.

## Solution

Least-privilege split in `crates/harnx-sandbox-common/src/defaults.rs`:

**On-PATH/host-executed directories → read+exec only:**
```rust
pub const HOME_EXEC_PATHS: &[&str] = &[
    ".local/bin", ".local/lib", ".bun", ".asdf", "go/bin", ".cargo",
    ".nvm", ".cargo/bin", ".mono", ".pyenv", ".rye",
    ".local/share/claude", ".local/share/opencode", ".local/share/pipx",
];
```

**Pure caches → read+write only:**
```rust
pub const HOME_WRITE_PATHS: &[&str] = &[
    ".cache", "go/pkg",
    ".npm", ".yarn", ".cargo/registry", ".cargo/git",
    ".bun/install/cache", ".local/share/pnpm", ".local/share/uv",
];
```

**RWX paths → emptied:**
```rust
pub const HOME_RWX_PATHS: &[&str] = &[];
```

**Opt-in for privileged operations:**
Users can grant write access to tool directories when needed:
- CLI: `--extra-rwx ~/.cargo/bin`
- Env: `HARNX_BASH_EXTRA_RWX=~/.cargo/bin`

**Alignment fix in sandbox-run:**
```rust
// sandbox.rs WRITE loop now pushes both flags
for path in HOME_WRITE_PATHS {
    args.push(OsString::from("--read"));
    args.push(expanded.clone());
    args.push(OsString::from("--write"));
    args.push(expanded);
}
```

## Why This Works

1. **Eliminates attack vector**: Directories on `$PATH` cannot be modified by sandboxed processes, preventing trojan injection.

2. **Preserves normal workflows**:
   - Networked builds/installations still work (caches keep write access)
   - Tool execution works (exec paths keep read+exec)
   - Self-updates and privileged installs require explicit opt-in

3. **Defense in depth**: Even if a sandboxed process is compromised, it cannot plant persistent backdoors in commonly-executed directories.

4. **Aligns with security principle**: "Never grant write to a directory whose contents the host will execute."

## Prevention Strategies

**Reusable heuristic:**
Never grant `write` permission to directories whose contents the host will execute. Apply this rule to any sandbox/containment system that shares directories with the host.

**Test Cases:**
- Assert exec-only paths receive `--read+--exec`, not `--write`
- Assert cache paths receive `--read+--write`, not `--exec`
- Assert `HOME_RWX_PATHS` is empty

**Best Practices:**
- Classify directories by execution risk before granting permissions
- Default to read-only for on-PATH directories
- Require explicit opt-in for security-sensitive permissions
- Audit default whitelists against `$PATH` and shell initialization

**Code Review Checklist:**
- [ ] Are on-PATH directories granted write access? (Should be exec-only)
- [ ] Are cache directories granted exec access? (Should be write-only)
- [ ] Is RWX empty or explicitly justified?
- [ ] Do tests verify least-privilege splits?

## Trade-offs

1. **Install/self-update workflows require explicit opt-in**: Users running `cargo install` or `npm install -g` inside sandbox must use `--extra-rwx`. This is intentional friction for security-sensitive operations.

2. **Normal builds still work**: Networked package downloads write to cache directories (`.npm`, `.cargo/registry`) which retain write access. Compilation happens in project directories or temp, not tool directories.

3. **Redundant entries**: `.cargo` and `.cargo/bin` both appear in exec list. `.cargo` parent covers all subdirs for exec, but explicit entries document intent and handle edge cases.

## Related Issues

- **Related Solution:** [sandbox-home-exposure-ancestor-walk-2026-05-21.md](sandbox-home-exposure-ancestor-walk-2026-05-21.md) — Previous sandbox hardening for write-path ancestor walk vulnerability
- **Plan:** sandbox-default-writable-hardening
