---
title: "Go Cache Auto-Whitelist and Arbitrary $VAR Path Expansion"
date: 2026-06-10
category: workflow-issues
problem_type: workflow_issue
component: harnx-sandbox-common
root_cause: "Go builds failed with read-only filesystem errors for custom cache dirs; path expansion syntax limited to pseudo-vars and tilde"
resolution_type: code_fix
severity: medium
tags:
  - sandboxing
  - go
  - path-expansion
  - environment-variables
  - security-invariant
plan_ref: harnx-666-go-cache-whitelist
---

## Problem

Go builds with custom `GOMODCACHE` or `GOCACHE` locations failed with "read-only file system" errors. Sandbox whitelist path arguments only supported pseudo-variables (`$GIT_ROOT`, `$CARGO_HOME`) and tilde — not arbitrary environment variables like `$GOMODCACHE`. Duplicated Go toolchain default-path logic between `harnx-sandbox-run` and `harnx-sandbox-common` created maintenance burden and behavior drift.

## Symptoms

- `go build` failed with "read-only file system" when `GOMODCACHE` or `GOCACHE` pointed to custom locations
- Whitelist paths like `$MY_CACHE_DIR` were treated as literal strings, not expanded
- First-run Go builds failed because cache directories didn't exist yet (existence checks blocked whitelist)
- `sandbox.rs` had 50+ lines of inline toolchain logic duplicated from `defaults.rs`

## Investigation Steps

1. Traced Go build failures to sandbox args missing cache dir whitelisting
2. Confirmed `push_env_relative_defaults` handles `CARGO_HOME`/`GOROOT`/`GOPATH`/`GOBIN` but not Go caches
3. Identified `.exists()` checks in `sandbox.rs` inline block that `defaults.rs` lacked — divergence bug
4. Reviewed Go cache semantics: source files, `.a` archives, build logs — no executables
5. Designed arbitrary `$VAR` expansion with prefix-boundary matching, following pseudo-var pattern
6. Resolved dedup by replacing inline block with call to shared `push_env_relative_defaults`

## Root Cause

1. **Missing Go cache whitelisting**: `push_env_relative_defaults` predated Go module/cache support. Go caches require read+write for downloads and build artifacts.

2. **Existence-check divergence**: `sandbox.rs` inline toolchain block gated whitelisting on `.exists()` — cache dirs don't exist before first `go build`. `defaults.rs` never had these checks, creating behavior drift.

3. **No arbitrary $VAR expansion**: `expand_path_var` only handled pseudo-vars and tilde. Users with custom cache paths (`$MY_PROJECT_CACHE`) had no convenient syntax.

## Solution

### 1. Auto-whitelist GOMODCACHE and GOCACHE (rw-only, no exec)

Added to `push_env_relative_defaults` in `defaults.rs`:

```rust
if let Some(gomodcache) = std::env::var_os("GOMODCACHE") {
    let gomodcache = PathBuf::from(gomodcache);
    // Go caches hold source, .a archives, and build logs only; test binaries
    // link and execute from $TMPDIR instead. Granting exec here would be a
    // security regression.
    args.push(OsString::from("--read"));
    args.push(gomodcache.clone().into_os_string());
    args.push(OsString::from("--write"));
    args.push(gomodcache.into_os_string());
}
// Similar for GOCACHE
```

Key decision: **rw-only, no exec**. Go caches contain no executables. Test binaries are linked and executed from `$TMPDIR`, not cache dirs. Granting exec would be a security regression.

**Unconditional whitelisting** — no `.exists()` check. Cache dirs often don't exist before first `go mod download`. That's the actual bug being fixed.

### 2. Deduplicate toolchain path logic

Replaced 50+ lines of inline Go/Rust toolchain logic in `sandbox.rs` with:

```rust
push_env_relative_defaults(&mut args);
```

This adopts the unconditional approach from `defaults.rs` for all toolchain env-relative paths (CARGO_HOME, GOROOT, GOPATH, GOBIN, GOMODCACHE, GOCACHE).

### 3. Arbitrary $VAR expansion

Extended `expand_path_var` in `path_expand.rs`:

```rust
// After pseudo-var loop, before tilde:
if let Some(stripped) = raw.strip_prefix('$') {
    let mut chars = stripped.char_indices();
    let Some((_, first)) = chars.next() else {
        return Some(PathBuf::from(expand_tilde(raw)));
    };
    if !first.is_ascii_alphabetic() && first != '_' {
        return Some(PathBuf::from(expand_tilde(raw)));
    }
    // Scan for valid env var name
    let end_pos = chars
        .find_map(|(i, c)| {
            if c.is_ascii_alphanumeric() || c == '_' { None } else { Some(i) }
        })
        .unwrap_or(stripped.len());
    let name = &stripped[..end_pos];
    let remainder = &stripped[end_pos..];
    // Boundary check: must be end or /
    if remainder.is_empty() || remainder.starts_with('/') {
        if let Some(value) = std::env::var_os(name) {
            let base = PathBuf::from(value);
            let suffix = remainder.strip_prefix('/').unwrap_or("");
            return Some(base.join(suffix));
        }
    }
    // Unset or bad boundary: fall through to literal
}
```

**Expansion precedence**: pseudo-vars → arbitrary `$VAR` → tilde → literal

**Boundary rules**:
- `$VAR` expands (end-of-string)
- `$VAR/sub` expands (followed by `/`)
- `$VAR-suffix` literal (followed by `-`)
- `$VARX` literal (different var name)
- `pre$VAR` literal (no `$` at start)
- `${VAR}` literal (brace syntax not supported)

**Unset vars**: return `Some(PathBuf::from(raw))` — literal passthrough. `None` reserved for pseudo-var detection failure.

### 4. Home guard stays at call sites

`expand_path_var` does NOT check `is_home_or_ancestor`. Call sites apply the guard after expansion:

```rust
// sandbox.rs extra_write handling:
let resolved = expand_path_var(path, &cwd).unwrap_or_else(|| PathBuf::from(path));
let resolved = resolve_path(&resolved);
if is_home_or_ancestor(&resolved) {
    eprintln!("harnx-sandbox-run: warning: ignoring --extra-write {}: would expose home directory", resolved.display());
    continue;
}
```

This prevents `$ENV_VAR_POINTING_TO_HOME` from bypassing home protection. Matches pattern from [sandbox-project-root-pseudo-vars-2026-06-02.md](sandbox-project-root-pseudo-vars-2026-06-02.md).

### 5. Forward Go cache env vars to child

Added `GOMODCACHE` and `GOCACHE` to both:

- `DEFAULT_PASSTHROUGH` in `sandbox.rs` (harnx-sandbox-run)
- `DEFAULT_ENV_ALLOWLIST` in `lifecycle.rs` (harnx-mcp-bash)

Ensures sandboxed `go` process sees custom cache locations matching whitelisted paths.

## Why This Works

**rw-only cache permissions**: Go caches contain source code (downloaded modules), compiled `.a` archives, and build logs. Test binaries are compiled to `$TMPDIR` and executed from there — never from cache dirs. Granting `--exec` would widen attack surface unnecessarily.

**Unconditional whitelisting**: First `go build` creates cache dir. If whitelisting required existence, initial builds would fail. Dropping `.exists()` matches actual usage pattern.

**Trust-the-user model for $VAR**: No allowlist for which env vars can expand. User controls sandbox process environment. Security comes from `is_home_or_ancestor` guard at call sites, not from expansion restrictions.

**Prefix-boundary matching**: Mirrors pseudo-var behavior. Prevents `$VARX` false expansion and mid-path expansion (not supported).

**Layered guard architecture**: Expansion is pure — returns `PathBuf` without side effects. Call sites are responsible for security checks. Makes expansion testable in isolation, makes security auditing easier (check call sites, not expansion internals).

## Prevention Strategies

**Test Cases:**
- Go cache whitelisting with rw-no-exec assertion
- `$VAR` expands to value, joins suffix correctly
- Unset var stays literal
- Boundary negatives: `$VARX`, `$VAR-suffix`, `pre$VAR`, `${VAR}`
- Home guard rejects `$ENV_VAR` expanding to `HOME`
- Pseudo-vars still take precedence over `$VAR` with same name prefix

**Best Practices:**
- Go cache paths get `--read` + `--write` only, never `--exec`
- New sandbox path sources MUST apply `is_home_or_ancestor` at call site
- Dropping `.exists()` for toolchain env-relative paths is intentional — first-run case
- `expand_path_var` tests: add new expansion type → add boundary tests

**Code Review Checklist:**
- [ ] Cache/package paths granted exec only if they contain executables?
- [ ] Unconditional whitelisting appropriate for this env-relative path?
- [ ] `$VAR` expansion expected behavior: literal on unset, boundary match only?
- [ ] Home guard applied after expansion at call site?

## Scope Limitation

`$VAR` expansion applies to CLI `--extra-*` flags, environment variable whitelist paths, and default toolchain paths.

> **Note (#850):** The per-call tool `inputs`/`outputs` parameters described below were removed from the bash MCP tools; this paragraph is retained for historical context only.

The (now removed) MCP per-call `inputs`/`outputs` parameters did NOT expand `$VAR` — they were literal paths validated against the working directory. `$VAR` expansion remains relevant only for the CLI `--extra-*` flags and toolchain paths noted above.

## Related Issues

- **GitHub Issue:** [#666 — Make it easy or automatic to whitelist write/execute to current golang cache](https://github.com/dobesv/harnx/issues/666)
- **Plan:** harnx-666-go-cache-whitelist
- **Related Solution:** [sandbox-project-root-pseudo-vars-2026-06-02.md](sandbox-project-root-pseudo-vars-2026-06-02.md) — pseudo-var expansion, prefix-boundary matching, home-guard checklist
- **Related Solution:** [sandbox-path-tilde-expansion-cross-platform-2026-04-29.md](sandbox-path-tilde-expansion-cross-platform-2026-04-29.md) — tilde expansion, non-Unix literal passthrough
- **Related Solution:** [../security-issues/environment-sanitization-bash-sandbox-2026-04-29.md](../security-issues/environment-sanitization-bash-sandbox-2026-04-29.md) — env var deny-by-default allowlist precedent
