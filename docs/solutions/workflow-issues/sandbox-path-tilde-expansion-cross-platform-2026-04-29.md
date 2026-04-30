---
title: "Sandbox Path Configuration: Tilde Expansion, Cross-Platform Flags, and Executable Roots"
date: 2026-04-29
category: workflow-issues
problem_type: workflow_issue
component: harnx-mcp-bash
root_cause: "Incomplete application of tilde expansion; inconsistent non-Unix flag handling; missing exec permission on sandbox roots"
resolution_type: code_fix
severity: medium
tags:
  - sandboxing
  - cross-platform
  - cli-parsing
  - environment-variables
  - tilde-expansion
  - proc-macros
  - dlopen
plan_ref: harnx-sandbox-roots-exec-extra-rwx
last_updated: 2026-04-29
---

## Problem

When adding `--extra-write`, `--extra-read`, `--extra-exec` path flags and their corresponding environment variables to `harnx-mcp-bash`, tilde expansion implementation was incomplete and non-Unix platforms lacked proper consume-and-ignore handling for cross-platform config compatibility.

Additionally, sandbox roots lacked execute permission, causing compiler toolchains and native extension loaders (e.g., Rust proc-macros, Python `.so` files) to fail with `dlopen` errors when running inside the sandbox.

## Symptoms

- `HARNX_BASH_EXTRA_WRITABLE=~/.cache` passed literal `~/.cache` to sandbox instead of expanding via `$HOME`
- Non-Unix platforms crashed with "unknown argument" when parsing shared config files containing sandbox flags
- **`cargo build` failed with `dlopen` errors for proc-macro `.so` files inside sandbox roots**:
  ```
  error: proc-macro derive produced error
  error: could not open proc-macro library: Permission denied
  ```
- **Native extensions (`.so`, `.dylib`) failed to load when built inside project root**
- **`--sandbox-run ~/bin/helper` failed to find helper binary** (tilde not expanded)
- Non-Unix (Windows) builds crashed with "unknown argument" for `--extra-read` and `--extra-exec` even though these flags were documented for shared configs
- Missing patterns in code review: `parse_env_paths()` lacked tilde expansion, incomplete non-Unix ignore handlers
- Changeset claimed tilde support for "corresponding environment variables" but implementation only covered CLI flags

## Investigation Steps

1. Initial implementation added `expand_tilde()` helper for CLI flag parsing arms
2. Code review identified: `parse_env_paths()` (lines 53-61) used `split_paths()` but never applied `expand_tilde()` to results
3. Review also found: non-Unix parser only special-cased `--extra-write`, leaving sibling sandbox flags to hit "unknown argument" fallback
4. Root cause: author implemented CLI path flow carefully, then missed parallel env/non-Unix compatibility surfaces
5. **User reported: `cargo build` inside sandbox failed with dlopen errors loading proc-macro `.so` files**
6. **Traced to: sandbox roots had write permission but not exec permission, blocking dynamic library loading**
7. **Second review found: `--sandbox-run` flag also missing tilde expansion; legacy flags (`--no-sandbox`, `--sandbox-run`) not ignored on non-Unix**

## Root Cause

**Tilde expansion gap:** `parse_env_paths()` function parsed environment variable paths via `std::env::split_paths()` and returned them unchanged. The `expand_tilde()` helper was only called in CLI flag match arms.

**Non-Unix parity gap:** Cross-platform config compatibility requires accepting sandbox-only flags on non-sandbox platforms (Windows). Only `--extra-write` had a consume-and-ignore handler; `--extra-read`, `--extra-exec`, `--no-sandbox`, and `--sandbox-run` fell through to the unknown-argument error path.

**Sandbox roots lacking exec permission:** When a sandbox hosts a compiler toolchain (e.g., Cargo building Rust code), the compiler writes `.so`/`.dylib` files into the project root and then attempts to `dlopen` them. Sandbox roots only had write permission — not execute permission — causing the `dlopen` to fail with "Permission denied". This breaks:
- Rust proc-macros compiled as `.so` files and loaded by `rustc`
- Python native extensions (`.so`) imported during test runs
- Any JIT compiler or plugin loader that writes code then maps it executable

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
3. Apply to ALL sandbox-only flags, not just new ones

**Critical completeness rule:** When adding a new sandbox flag on Unix, you MUST add a consume-and-ignore handler on the non-Unix branch. Otherwise shared configs crash on Windows. This applies to legacy flags too — `--no-sandbox` and `--sandbox-run` were forgotten in the initial implementation.

**Update to include all sandbox flags (main.rs non-Unix branch):**
```rust
"--extra-rwx" => {
    if i + 1 < args.len() {
        i += 2;
    } else {
        eprintln!("harnx-mcp-bash: --extra-rwx requires a path argument");
        std::process::exit(1);
    }
}
"--no-sandbox" => {
    i += 1;  // flag only, no value
}
"--sandbox-run" => {
    if i + 1 < args.len() {
        i += 2;
    } else {
        eprintln!("harnx-mcp-bash: --sandbox-run requires a path argument");
        std::process::exit(1);
    }
}
```

### 3. Grant execute permission on sandbox roots for compiler toolchains

**The failure mode:** Compilers like `cargo build` write compiled artifacts (`.so`, `.dylib`) into the project directory, then immediately attempt to load them via `dlopen` for proc-macros, native extensions, or JIT code. If the sandbox grants write but not execute on the root, the `dlopen` fails:

```
error: could not open proc-macro library: Permission denied
```

**Fix in build_sandbox_args (server.rs:652-686):**

```rust
// In outputs: None branch (default full access)
for root in roots {
    args.push(OsString::from("--write"));
    args.push(root.clone().into_os_string());
    args.push(OsString::from("--exec"));
    args.push(root.clone().into_os_string());
}

// In outputs: Some([]) and outputs: Some(paths) branches
// Grant read+exec to roots so compilers can dlopen built artifacts
for root in roots {
    args.push(OsString::from("--read"));
    args.push(root.clone().into_os_string());
    args.push(OsString::from("--exec"));
    args.push(root.clone().into_os_string());
}
```

**Pattern:** Any sandbox hosting a compiler toolchain must grant exec on writable directories where the compiler will produce loadable artifacts.

### 4. `--extra-rwx` for paths needing full read/write/execute access

**When to use vs separate flags:**
- `--extra-rwx ~/.cargo` — Cargo registries contain `.crate` archives (read), build scripts write to target dirs (write), and proc-macros are loaded via dlopen (exec)
- `--extra-write` + `--extra-exec` — Use when a path needs write+exec but NOT read (rare; usually exec implies read)
- `--extra-read` + `--extra-exec` — Use when a path has pre-built binaries that shouldn't be modified

**Implementation (main.rs Unix branch):**
```rust
"--extra-rwx" => {
    if i + 1 < args.len() {
        sandbox_config
            .extra_rwx
            .push(PathBuf::from(expand_tilde(&args[i + 1])));
        i += 2;
    } else {
        eprintln!("harnx-mcp-bash: --extra-rwx requires a path argument");
        std::process::exit(1);
    }
}
```

**In build_sandbox_args (server.rs:640-646):**
```rust
for path in &self.sandbox_config.extra_rwx {
    args.push(OsString::from("--read"));
    args.push(path.clone().into_os_string());
    args.push(OsString::from("--write"));
    args.push(path.clone().into_os_string());
    args.push(OsString::from("--exec"));
    args.push(path.clone().into_os_string());
    readable_paths.push(path.clone());
    writable_paths.push(path.clone());
}
```

### 5. Tilde expansion for `--sandbox-run` helper path

**The gap:** All other path flags used `expand_tilde()`, but `--sandbox-run` was missed:

```rust
// BEFORE: literal ~/bin/helper fails
"--sandbox-run" => {
    sandbox_run_override = Some(PathBuf::from(&args[i + 1]));  // BUG: no tilde expansion
    i += 2;
}

// AFTER: correctly expands home directory
"--sandbox-run" => {
    sandbox_run_override = Some(PathBuf::from(expand_tilde(&args[i + 1])));
    i += 2;
}
```

### 6. Platform-specific default paths via `#[cfg]` helper

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

**Consume-and-ignore completeness:** Shared config files (e.g., dotfiles, team-wide configs) may include sandbox flags. On non-Unix platforms without sandbox support, these must be accepted to prevent startup failure. The `i += 2` pattern prevents the next flag from being misread as the value argument. Legacy flags like `--no-sandbox` and `--sandbox-run` must also be handled — adding a new sandbox flag on Unix requires remembering the non-Unix counterpart.

**Platform-specific defaults:** System temp directories differ by OS. The `#[cfg]` helper pattern ensures compile-time correctness without runtime branching overhead.

**Roots require exec for compilers:** Compiler toolchains (Cargo, rustc) write `.so`/`.dylib` artifacts and immediately `dlopen` them for proc-macros. Granting write but not execute on sandbox roots causes "Permission denied" at the dlopen step, even though the file exists and is readable. The fix grants `--exec` alongside `--write` on roots.

**`--extra-rwx` convenience:** Paths like `~/.cargo` need all three permissions (read crates, write build artifacts, exec proc-macros). A single flag is less error-prone than repeating the path three times.

**Test isolation:** `std::env::set_var` is unsafe and process-global. Mutex ensures no concurrent tests mutate env vars simultaneously. RAII pattern guarantees restoration even if test panics.

## Prevention Strategies

**Test Cases:**
- Add tests for `parse_env_paths()` with tilde-prefixed paths
- Add tests asserting all sandbox flags (including legacy) are consumed on non-Unix without error
- Test with `HOME` unset to verify graceful fallback
- Add test asserting roots get `--exec` flag in sandbox args
- Add test asserting `extra_rwx` paths get all three permission flags

**Best Practices:**
- When implementing path transformations, search for ALL sites where paths enter the system (CLI, env vars, config files)
- For cross-platform flags, maintain a checklist of ALL related flags needing consume-and-ignore handlers, including legacy ones
- Use `grep` or AST tools to verify all call sites receive transformation
- When granting write permission on a directory, ask: "Will anything need to execute files written here?" If yes, grant exec too.
- Sandbox roots hosting compiler toolchains always need exec permission

**Code Review Checklist:**
- [ ] Are CLI flags and env vars handled consistently?
- [ ] Does non-Unix parser consume-and-ignore ALL sandbox-only flags (new AND legacy)?
- [ ] Are platform-specific defaults cleanly separated via `#[cfg]`?
- [ ] Do tests serialize env mutations and restore original values?
- [ ] Do sandbox roots get exec permission, not just write?
- [ ] Have all path-bearing flags been checked for tilde expansion?

**Windows Consume-and-Ignore Completeness Rule:**

When adding a new Unix-only sandbox flag, check the non-Unix `parse_args` branch. The checklist of flags to ignore must remain complete. Example flags requiring consume-and-ignore on non-Unix:
- `--extra-read`, `--extra-write`, `--extra-exec`, `--extra-rwx` (value-bearing)
- `--no-sandbox`, `--sandbox-run` (legacy, easy to forget)
- `--root`, `--sandbox-run` path argument (value-bearing)

Missing any of these causes "unknown argument" crash on Windows when using shared configs.
