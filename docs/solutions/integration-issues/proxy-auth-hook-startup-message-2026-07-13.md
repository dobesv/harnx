---
title: "Proxy auth exec-hook startup message protocol and env precedence"
date: 2026-07-13
category: integration-issues
problem_type: integration_issue
component: harnx-proxy-auth
root_cause: "environment variable precedence split between process env and extra_env map; unsafe set_var timing after Tokio spawn"
resolution_type: code_fix
severity: high
tags:
  - proxy-auth
  - exec-hooks
  - environment-variables
  - precedence
  - tokio
  - rust-2024
  - startup-protocol
  - jsonl
plan_ref: "proxy-auth-hook-startup-message"
---

## Problem

`harnx-proxy-auth` exec hooks could not inject environment variables or write files before the first HTTP request. The startup message protocol was added, but the initial implementation had a **precedence bug**: startup env vars were applied to both process env (`std::env::set_var`) and the `extra_env` map using `or_insert`, causing split-brain when a key also existed from `--env` jaq scripts.

## Symptoms

- Hook-provided env vars (e.g., `ACLI_CONFIG_DIR`) appeared in sandboxed tool env but not in proxy process env accessible to jaq filters
- Tests showed that `--env` jaq vars should win over hook startup vars, but process env had the hook value while `extra_env` had the jaq value
- Precedence invariant documented as `tool_input.env > --env jaq > hook startup env > proxy defaults` was violated
- Rust 2024 edition marks `std::env::set_var` as unsafe due to data races with concurrent threads

## Investigation Steps

1. Reviewed `main.rs` startup sequence: `build_stages` → `start_proxy_with_log` → `eval_env_scripts` → `run_startup` → env merge
2. Found that startup env was applied unconditionally:
   - `std::env::set_var(&key, value)` called for every startup env key
   - `extra_env.entry(key).or_insert(value)` called after
   - When jaq `--env` already set the key, `or_insert` kept jaq value in `extra_env` (correct for sandbox) but `set_var` had already clobbered process env with hook value (wrong for jaq filters)
3. Traced the fix: skip both `set_var` and `insert` when key already in `extra_env`:
   ```rust
   for (key, value) in startup_env {
       if extra_env.contains_key(&key) {
           continue;
       }
       unsafe { std::env::set_var(&key, value.as_str().expect("startup env strings only")); }
       extra_env.insert(key, value);
   }
   ```
4. Audited Tokio spawn timing: `start_proxy_with_log` spawns listener tasks before startup env mutation begins
5. Verified via grep that proxy background tasks do not read/write process env during the startup window

## Root Cause

**Precedence bug:** The startup env merge called `set_var` before checking if a key already existed in `extra_env`. The `or_insert` on `extra_env` correctly preserved jaq values, but `set_var` unconditionally overwrote process env. This split the environment: jaq filters saw hook values via `env.VAR`, while sandboxed tools saw jaq values via `extra_env`.

**Unsafe timing:** Rust 2024 marks `std::env::set_var` as unsafe because modifying process env after threads exist is UB if those threads also access env vars. The `start_proxy_with_log` function spawns Tokio tasks before the startup env mutation window, so the "single-threaded" safety justification is false. The code is safe in practice only because the spawned proxy tasks do not touch process env during this window.

## Solution

### 1. Correct Precedence: Check Before Both set_var and insert

**Before (buggy):**
```rust
for (key, value) in startup_env {
    unsafe { std::env::set_var(&key, value.as_str().expect("...")); }
    extra_env.entry(key).or_insert(value);
}
```

**After (fixed):**
```rust
for (key, value) in startup_env {
    if extra_env.contains_key(&key) {
        continue;  // jaq --env wins; skip both set_var and insert
    }
    unsafe { std::env::set_var(&key, value.as_str().expect("...")); }
    extra_env.insert(key, value);
}
```

### 2. Honest SAFETY Comment for set_var

The SAFETY comment now accurately describes the real invariant:

```rust
// SAFETY: startup hooks run after proxy listener tasks start but before
// JSONL request loop begins. In this window, harnx-proxy-auth startup
// path is sole code mutating process env, and proxy/background tasks
// spawned by start_proxy_with_log do not read or write process env.
// That keeps this mutation race-free in practice for current code.
unsafe {
    std::env::set_var(&key, value.as_str().expect("startup env strings only"));
}
```

### 3. Startup Message Protocol

**Message format (proxy → hook):**
```json
{"id": "evt-<n>", "event": "startup", "vars": {"temp_file_root": "/tmp/harnx-fs-...", "proxy_port": 12345, ...}}
```

**Response format (hook → proxy):**
```json
{"id": "evt-<n>", "env": {"ACLI_CONFIG_DIR": "/tmp/harnx-fs-.../acli", ...}}
```

**Timeout handling:** If hook doesn't respond within `hook_timeout_secs`, warn and continue with empty env contribution.

**Backwards compatibility:** Hooks that ignore `event:"startup"` echo it back with no `env` key → treated as empty contribution.

### 4. String-Only env Values

`extract_startup_env` filters the response to keep only string values:

```rust
let mut env = Map::new();
for (key, value) in env_obj {
    if matches!(value, Value::String(_)) {
        env.insert(key, value);
    }
}
```

Non-string values (numbers, objects) are silently dropped rather than causing injection failures downstream.

### 5. Eager Hook Spawn

Exec hooks now spawn eagerly via `ensure_runtime()` called explicitly during startup, not lazily on first `transform()`:

```rust
// TransformPipeline::run_startup iterates exec stages
for stage in &self.stages {
    if let Stage::Exec(process) = stage {
        for (key, value) in process.startup(vars.clone(), timeout).await {
            merged.entry(key).or_insert(value);  // earlier stage wins
        }
    }
}
```

## Why This Works

**Precedence check:** By skipping both `set_var` and `insert` when the key exists in `extra_env`, the jaq `--env` value wins in both process env (for jaq filters) and `extra_env` (for sandboxed tools). The precedence order is enforced correctly: `tool_input.env > --env jaq > hook startup env > proxy defaults`.

**Honest invariant:** The SAFETY comment no longer claims "single-threaded" execution. Instead, it specifies what actually makes the mutation safe: no concurrent env access during the startup window. This is verified by grep'ing the proxy code for env reads/writes.

**Protocol design:** The startup message provides `temp_file_root` and `proxy_port` to hooks, enabling config file generation before the sandboxed command runs. The `env` response injects vars into both process env and `extra_env`.

**Graceful degradation:** Hooks without startup support continue to work — the startup message is silently ignored, and lazy init in `handle_request()` remains as fallback.

## Prevention Strategies

**Test Cases:**
- Add test verifying `extract_startup_env` keeps only strings, drops non-strings silently
- Test startup timeout returns empty map without failing
- Test that `run_startup` merges with earlier-stage precedence (first stage wins for duplicate keys)
- Test that jaq `--env` wins over hook startup env for the same key

**Best Practices:**
- When env vars flow through multiple paths (process env + map), enforce precedence in ALL paths atomically
- Document the real invariant for unsafe operations — false safety justifications are worse than none
- Add startup protocol messages after `READY` line, not interleaved with request handling
- Use `contains_key` check before both mutation paths (set_var + map insert) to avoid split-brain

**Code Review Checklist:**
- [ ] Does the precedence check guard ALL mutation paths (process env + map)?
- [ ] Is the SAFETY comment for `unsafe` blocks honest about actual invariants?
- [ ] Do spawned background tasks avoid env access during mutable windows?
- [ ] Are startup protocol tests covering timeout, malformed response, and missing `env` key?
- [ ] Does `extract_startup_env` validate/filter env values before injection?

## Related Issues

- **Issue:** [#1049](https://github.com/dobesv/harnx/issues/1049) — Allow auth proxy hooks to set env vars and write files before the command runs
- **Solution:** [sentinel-env-vars-proxy-injection-2026-05-27.md](./sentinel-env-vars-proxy-injection-2026-05-27.md) — Env var injection via `std::env::set_var` after `eval_env_scripts`
- **Solution:** [atlassian-cli-proxy-auth-2026-05-20.md](./atlassian-cli-proxy-auth-2026-05-20.md) — acli auth flow and synthetic config file writing
