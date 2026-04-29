---
title: "Environment Variable Sanitization for Sandboxed Child Processes"
date: 2026-04-29
category: security-issues
problem_type: security_issue
component: harnx-mcp-bash
root_cause: "Full host environment leaked to sandboxed child processes via Exception::FullEnvironment"
resolution_type: code_fix
severity: high
tags:
  - environment-variables
  - sandboxing
  - birdcage
  - security
  - cross-platform
plan_ref: gh-375-bash-env-sanitization
---

## Problem

Bash child processes inherited the full host environment, exposing secrets (AWS keys, API tokens, SSH keys) to potentially compromised or malicious agent-controlled commands. Original implementation used `Exception::FullEnvironment` which bypassed all env restrictions in birdcage sandbox.

## Symptoms

These symptoms describe the original implementation before the fix in PR #381 (gh-375), where `Exception::FullEnvironment` allowed the birdcage sandbox to inherit the full host environment, the `--no-sandbox` path inherited host env unchanged, and `#[cfg(unix)]`-gated env curation left Windows builds without any sanitization.

- `AWS_SECRET_ACCESS_KEY`, `GITHUB_TOKEN`, and other secrets visible to bash commands via `echo $VAR`
- Non-sandboxed path (`--no-sandbox`) inherited full host environment unchanged
- Windows builds bypassed env curation entirely via `#[cfg(unix)]` guards

## Investigation Steps

1. Traced env handling through both sandboxed and non-sandboxed spawn paths
2. Discovered `Exception::FullEnvironment` allowed all vars through
3. Investigated birdcage 0.8.1 API: `birdcage::process::Command` lacks `env()`, `envs()`, `env_clear()` methods — cannot set env on Command directly
4. Found birdcage's `restrict_env_variables()` iterates `std::env::vars()` and removes non-excepted vars at sandbox lock time
5. Identified `#[cfg(unix)]` guard on env curation leaked full env on Windows/non-Unix builds

## Root Cause

Two separate issues:

1. **birdcage environment model**: `birdcage::process::Command` (a wrapper around `std::process::Command`) does not expose `env`, `envs`, or `env_clear`, so child env cannot be set on the Command directly. The required workaround is two-step: (a) call `std::env::set_var()` on the current process to stage each desired value before invoking `sandbox.spawn()`, and (b) add an `Exception::Environment(name)` entry to the sandbox for every var that should survive. Birdcage's `restrict_env_variables()` (invoked from inside `spawn`) then iterates `std::env::vars()` and removes everything except the names listed via `Exception::Environment` — it operates on the current process env, not on the Command.

2. **Platform-gated env curation**: Env sanitization was `#[cfg(unix)]`-guarded, matching the sandboxing code. This meant Windows builds inherited full host env even for non-sandboxed spawns.

## Solution

### 1. Replace `Exception::FullEnvironment` with per-var exceptions

**Before (sandbox_run.rs):**
```rust
sandbox
    .add_exception(Exception::FullEnvironment)
    .map_err(|error| {
        format!("sandbox-run: failed to add FullEnvironment exception: {error}")
    })?;
```

**After:**
```rust
for (key, value) in &config.env_vars {
    // SAFETY: `env::set_var` is unsafe because it mutates process-global
    // state and is not thread-safe. This binary is the `sandbox_run` helper,
    // which runs single-threaded up to this point — `parse_args` and the
    // sandbox setup never spawn threads, and we have not yet called
    // `sandbox.spawn(...)`. No other code in the process can be observing
    // the environment concurrently, so the call is sound. We must do this
    // before `sandbox.spawn(...)` because birdcage's `restrict_env_variables()`
    // (invoked from `Birdcage::lock` inside `spawn`) inspects `std::env::vars()`
    // and removes any variable not listed via `Exception::Environment`.
    unsafe { env::set_var(key, value) };
    sandbox
        .add_exception(Exception::Environment(key.clone()))
        .map_err(|error| {
            format!("sandbox-run: failed to add env exception for {key}: {error}")
        })?;
}
```

### 2. Add curated default allowlist

```rust
const DEFAULT_ENV_ALLOWLIST: &[&str] = &[
    "HOME", "PATH", "LANG", "LANGUAGE", "USER", "SHELL", "TERM",
    "DISPLAY", "EDITOR", "NODE_OPTIONS", "NODE_EXTRA_CA_CERTS",
    "PWD", "SHLVL", "LOGNAME", "TMPDIR", "TMP", "TEMP",
];
```

Plus `XDG_*` prefix pattern expanded at runtime.

### 3. Layered env configuration with explicit precedence

**Precedence (CLI > Passthrough > Dotfile > XDG_* > Allowlist):**

`XDG_*` is treated as part of the default allowlist layer: applied after `DEFAULT_ENV_ALLOWLIST` but before `.env.bash` (so dotfile values override `XDG_*`).

```rust
fn build_child_env(&self) -> Vec<(String, String)> {
    fn upsert(env_vars: &mut Vec<(String, String)>, key: String, value: String) {
        if let Some((_, existing)) = env_vars.iter_mut().find(|(k, _)| k == &key) {
            *existing = value;
        } else {
            env_vars.push((key, value));
        }
    }

    let mut env_vars: Vec<(String, String)> = Vec::new();

    // 1. Default allowlist (lowest precedence)
    for name in Self::DEFAULT_ENV_ALLOWLIST {
        if let Ok(value) = std::env::var(name) {
            upsert(&mut env_vars, (*name).to_string(), value);
        }
    }

    // 2. XDG_* vars from host env
    for (name, value) in std::env::vars() {
        if name.starts_with("XDG_") {
            upsert(&mut env_vars, name, value);
        }
    }

    // 3. .env.bash dotfile
    for (key, value) in load_bash_env_file() {
        upsert(&mut env_vars, key, value);
    }

    // 4. Explicit passthrough names — host value wins over dotfile
    for name in &self.inner.sandbox_config.extra_env_passthrough {
        if let Ok(value) = std::env::var(name) {
            upsert(&mut env_vars, name.clone(), value);
        }
    }

    // 5. Explicit overrides — highest precedence
    for (key, value) in &self.inner.sandbox_config.env_overrides {
        upsert(&mut env_vars, key.clone(), value.clone());
    }

    env_vars
}
```

### 4. Make env curation cross-platform

Removed `#[cfg(unix)]` guard from `build_child_env()`. Applied to non-sandboxed spawn path on all platforms:

```rust
let child_env = self.build_child_env();
CommandWrap::with_new("bash", |command| {
    command
        .args(["-c", &params.command])
        .current_dir(&working_dir)
        .stdin(Stdio::null());
    command.env_clear();
    command.envs(child_env.iter().map(|(k, v)| (k, v)));
    // ...
})
```

### 5. Process-global env mutation in tests

Tests that mutate `std::env` require serialization. Pattern:

```rust
#[cfg(unix)]
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    match LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(unix)]
struct EnvVar {
    key: String,
    prev: Option<OsString>,
}

#[cfg(unix)]
impl EnvVar {
    fn set(key: &str, value: impl AsRef<OsStr>) -> Self {
        let prev = std::env::var_os(key);
        unsafe { std::env::set_var(key, value.as_ref()) };
        Self { key: key.to_string(), prev }
    }

    fn unset(key: &str) -> Self {
        let prev = std::env::var_os(key);
        unsafe { std::env::remove_var(key) };
        Self { key: key.to_string(), prev }
    }
}

#[cfg(unix)]
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
```

Usage: `let _guard = env_lock(); let _var = EnvVar::set("KEY", "value");` — guard serializes, RAII restores.

## Why This Works

1. **birdcage env model**: Setting vars on current process before `spawn()` works because birdcage's internal `restrict_env_variables()` inspects `std::env::vars()` at lock time. The child inherits the sanitized env from the wrapper process.

2. **Cross-platform curation**: Env sanitization is now independent of sandbox enforcement. Windows, macOS, Linux all apply the same env curation — only the sandbox activation layer remains Unix-only.

3. **Ordered Vec with upsert**: Preserves insertion order (reflects precedence layers) while allowing later layers to replace values. Simpler than HashMap for this use case.

## Prevention Strategies

**Test Cases:**
- Verify secrets (`AWS_SECRET_ACCESS_KEY`) not leaked to child processes
- Verify allowlist vars (`HOME`, `PATH`) passed through
- Verify precedence: CLI override beats passthrough beats dotfile beats default

**Best Practices:**
- When implementing security controls with sandboxed and non-sandboxed paths, make sanitization layer platform-independent
- Document and test precedence explicitly when multiple config sources exist
- For birdcage: understand that `Exception::Environment` + `set_var` on current process is the only way to control child env

**Code Review Checklist:**
- [ ] Are secrets prevented from reaching child processes?
- [ ] Is env curation applied on all platforms, not just sandbox-capable ones?
- [ ] Is precedence documented and tested?
- [ ] If using birdcage, are env vars set on current process before `spawn()`?

## Related Issues

- **GitHub Issue:** [#375 — Environment sanitization for bash MCP server](https://github.com/dobesv/harnx/issues/375)
- **Plan:** gh-375-bash-env-sanitization
- **Commit:** 9161557 — feat(bash): restrict and curate child process environment
- **Related Solution:** [integration-issues/cli-wrapper-sandboxing-for-tokio-servers-2026-04-28.md](../integration-issues/cli-wrapper-sandboxing-for-tokio-servers-2026-04-28.md) — CLI wrapper pattern for birdcage sandboxing
