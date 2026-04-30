---
title: "Tilde Expansion and Cross-Platform CLI Patterns for Sandbox Path Flags"
date: 2026-04-29
category: workflow-issues
problem_type: workflow_issue
component: harnx-mcp-bash
root_cause: "Incomplete application of tilde expansion across CLI and env-var paths; inconsistent non-Unix flag handling"
resolution_type: code_fix
severity: medium
tags:
  - sandboxing
  - cross-platform
  - cli-parsing
  - environment-variables
  - tilde-expansion
plan_ref: harnx-sandbox-extra-write-tempdir-tilde
---

## Problem

When adding `--extra-write`, `--extra-read`, `--extra-exec` path flags and their corresponding environment variables to `harnx-mcp-bash`, tilde expansion implementation was incomplete and non-Unix platforms lacked proper consume-and-ignore handling for cross-platform config compatibility.

## Symptoms

- `HARNX_BASH_EXTRA_WRITABLE=~/.cache` passed literal `~/.cache` to sandbox instead of expanding via `$HOME`
- Non-Unix (Windows) builds crashed with "unknown argument" for `--extra-read` and `--extra-exec` even though these flags were documented for shared configs
- Missing patterns in code review: `parse_env_paths()` lacked tilde expansion, incomplete non-Unix ignore handlers
- Changeset claimed tilde support for "corresponding environment variables" but implementation only covered CLI flags

## Investigation Steps

1. Initial implementation added `expand_tilde()` helper for CLI flag parsing arms
2. Code review identified: `parse_env_paths()` (lines 53-61) used `split_paths()` but never applied `expand_tilde()` to results
3. Review also found: non-Unix parser only special-cased `--extra-write`, leaving sibling sandbox flags to hit "unknown argument" fallback
4. Root cause: author implemented CLI path flow carefully, then missed parallel env/non-Unix compatibility surfaces

## Root Cause

**Tilde expansion gap:** `parse_env_paths()` function parsed environment variable paths via `std::env::split_paths()` and returned them unchanged. The `expand_tilde()` helper was only called in CLI flag match arms.

**Non-Unix parity gap:** Cross-platform config compatibility requires accepting sandbox-only flags on non-sandbox platforms (Windows). Only `--extra-write` had a consume-and-ignore handler; `--extra-read` and `--extra-exec` fell through to the unknown-argument error path.

## Solution

### 1. Apply `expand_tilde()` in both CLI arms AND `parse_env_paths()`

**Before (main.rs:53-60):**
```rust
fn parse_env_paths(var_name: &str) -> Vec<PathBuf> {
    std::env::var_os(var_name)
        .map(|value| {
            std::env::split_paths(&value)
                .filter(|path| !path.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default()
}
```

**After:**
```rust
fn parse_env_paths(var_name: &str) -> Vec<PathBuf> {
    std::env::var_os(var_name)
        .map(|value| {
            std::env::split_paths(&value)
                .filter(|path| !path.as_os_str().is_empty())
                .map(|path| PathBuf::from(expand_tilde(&path.to_string_lossy())))
                .collect()
        })
        .unwrap_or_default()
}
```

**Pattern:** When adding path transformation logic (tilde expansion, canonicalization, etc.), apply it consistently in:
1. CLI flag parsing arms (immediate call site)
2. Environment variable parsing functions (easy to miss)

### 2. Consume-and-ignore pattern for non-Unix sandbox flags

**Non-Unix parse_args (main.rs:330-337):**
```rust
"--extra-read" => {
    if i + 1 < args.len() {
        i += 2; // consume flag and value, ignore both
    } else {
        eprintln!("harnx-mcp-bash: --extra-read requires a path argument");
        std::process::exit(1);
    }
}
"--extra-exec" => {
    if i + 1 < args.len() {
        i += 2;
    } else {
        eprintln!("harnx-mcp-bash: --extra-exec requires a path argument");
        std::process::exit(1);
    }
}
"--extra-write" => {
    if i + 1 < args.len() {
        i += 2;
    } else {
        eprintln!("harnx-mcp-bash: --extra-write requires a path argument");
        std::process::exit(1);
    }
}
```

**Key insight:** Consume-and-ignore must:
1. Increment `i` by 2 (flag + value) to avoid desynchronizing argv parsing
2. Still validate that a value argument exists (error on missing value)
3. Apply to ALL sandbox-only flags, not just one

### 3. Platform-specific default paths via `#[cfg]` helper

**system_writable_paths() pattern (server.rs:372-393):**
```rust
#[cfg(unix)]
fn system_writable_paths() -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        vec![PathBuf::from("/tmp")]
    }
    #[cfg(target_os = "macos")]
    {
        let mut paths = vec![PathBuf::from("/private/tmp")];
        if let Ok(tmpdir) = std::env::var("TMPDIR") {
            let path = PathBuf::from(&tmpdir);
            if path != PathBuf::from("/private/tmp") {
                paths.push(path);
            }
        }
        paths
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        vec![PathBuf::from("/tmp")]
    }
}
```

**Pattern:** Nested `#[cfg]` conditionals within a single function provide:
1. Compile-time elimination of unreachable branches
2. Type-safe return value across all Unix targets
3. Fallback catch-all for future Unix-like platforms

### 4. Environment variable test isolation via Mutex + RAII

**Pattern (main.rs:410-460):**
```rust
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    match LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

struct EnvVar {
    key: String,
    prev: Option<OsString>,
}

impl EnvVar {
    fn set(key: &str, value: impl AsRef<OsStr>) -> Self {
        let prev = std::env::var_os(key);
        unsafe { std::env::set_var(key, value.as_ref()) };
        Self { key: key.to_string(), prev }
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        unsafe {
            match self.prev.take() {
                Some(value) => std::env::set_var(&self.key, value),
                None => std::env::remove_var(&self.key),
            }
        }
    }
}

#[test]
fn test_expand_tilde_replaces_prefix() {
    let _env_guard = env_lock();         // serialize env access
    let _home = EnvVar::set("HOME", "/tmp/test-home");  // RAII restore
    assert_eq!(expand_tilde("~/foo"), "/tmp/test-home/foo");
}
```

**Pattern components:**
- `OnceLock<Mutex<()>>` provides process-wide serialization for tests mutating env
- RAII `EnvVar` guard saves previous value, restores on drop
- `unsafe` blocks required for `set_var`/`remove_var` (documented UB in Rust 2024)

## Why This Works

**Tilde expansion in both sites:** Environment variables are parsed before CLI flags are fully processed. Missing expansion there means `HARNX_BASH_EXTRA_WRITABLE=~/.cache` creates a `PathBuf` from literal string, which sandbox interprets as relative path starting with `~`.

**Consume-and-ignore parity:** Shared config files (e.g., dotfiles, team-wide configs) may include sandbox flags. On non-Unix platforms without sandbox support, these must be accepted to prevent startup failure. The `i += 2` pattern prevents the next flag from being misread as the value argument.

**Platform-specific defaults:** System temp directories differ by OS. The `#[cfg]` helper pattern ensures compile-time correctness without runtime branching overhead.

**Test isolation:** `std::env::set_var` is unsafe and process-global. Mutex ensures no concurrent tests mutate env vars simultaneously. RAII pattern guarantees restoration even if test panics.

## Prevention Strategies

**Test Cases:**
- Add tests for `parse_env_paths()` with tilde-prefixed paths
- Add tests asserting all sandbox flags are consumed on non-Unix without error
- Test with `HOME` unset to verify graceful fallback

**Best Practices:**
- When implementing path transformations, search for ALL sites where paths enter the system (CLI, env vars, config files)
- For cross-platform flags, maintain a checklist of all related flags needing consume-and-ignore handlers
- Use `grep` or AST tools to verify all call sites receive transformation

**Code Review Checklist:**
- [ ] Are CLI flags and env vars handled consistently?
- [ ] Does non-Unix parser consume-and-ignore ALL sandbox-only flags?
- [ ] Are platform-specific defaults cleanly separated via `#[cfg]`?
- [ ] Do tests serialize env mutations and restore original values?
