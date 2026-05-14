---
title: "Per-call env parameter for bash_exec and bash_spawn MCP tools"
date: 2026-05-13
category: api-design
problem_type: integration_issue
component: harnx-mcp-bash
root_cause: "No mechanism for per-call environment variable injection in bash execution tools"
resolution_type: code_fix
severity: medium
tags:
  - environment-variables
  - mcp-tools
  - sandboxing
  - rust
  - json-schema
plan_ref: harnx-532-bash-env-args
---

## Problem

Bash MCP tools (`bash_exec`, `bash_spawn`) lacked a mechanism to inject per-call environment variables, forcing callers to either modify global server config or wrap commands with inline env assignments (`VAR=value command`).

## Symptoms

- Agents and hooks could not customize environment for individual commands without affecting other concurrent executions
- Workarounds like `ENV_VAR=value bash -c "command"` were fragile and didn't integrate with sandbox mode
- No API parity with typical process-spawning libraries that accept `env` maps

## Investigation Steps

1. Reviewed `ExecCommandParams` and `SpawnCommandParams` structs in `server.rs` — identified pattern for optional parameters using `#[serde(default)]`
2. Traced execution paths: sandbox path uses `--env` flags to `sandbox-run`, non-sandbox path uses `std::process::Command::envs()`
3. Checked existing `build_child_env()` implementation — returns ordered `(String, String)` pairs from server's curated environment
4. Verified `sandbox-run` already supports `--env KEY=VALUE` flag (used by environment sanitization layer)
5. Reviewed existing tests pattern: `sandbox_runtime_works()` guard for Linux-only sandbox tests with early `return`

## Root Cause

MCP tool parameters lacked an `env` field to pass per-call environment overrides. The sandbox and non-sandbox execution paths needed different injection mechanisms:
- **Sandbox**: `--env KEY=VALUE` flags appended to `sandbox-run` args (before `--` separator)
- **Non-sandbox**: `command.envs()` called after `build_child_env()` to layer overrides on top

## Solution

Added `env: Option<HashMap<String, String>>` to both parameter structs with integration across execution paths.

### Struct Addition

```rust
#[derive(Debug, Deserialize)]
struct ExecCommandParams {
    // ... existing fields ...
    #[serde(default)]
    env: Option<HashMap<String, String>>,
}
```

### Schema Documentation

Used `object_schema_with_desc` helper for schema generation with description:

```rust
("env", "Additional environment variables for the command. Merged on top of the server's environment; per-call overrides only.", env),
```

### Sandbox Path (Unix)

Before `--` separator in `sb_args`, append `--env` flags:

```rust
if let Some(ref extra_env) = params.env {
    for (key, value) in extra_env {
        sb_args.push(OsString::from("--env"));
        sb_args.push(OsString::from(format!("{key}={value}")));
    }
}
sb_args.push(OsString::from("--"));  // separator must come AFTER env flags
```

**Critical**: `--env` flags must appear before `--` separator so `sandbox-run` receives them as flags, not as bash arguments.

### Non-Sandbox Path

Layer on top of `child_env` using `command.envs()`:

```rust
let extra_env = params.env.clone().unwrap_or_default();
command.env_clear();
command.envs(child_env.iter());
command.envs(extra_env.iter());
```

Order matters: `extra_env` applied last so it overrides `child_env` entries.

### Test Pattern for Sandbox Tests

```rust
#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_sandbox_exec_per_call_env_vars() {
    if !sandbox_runtime_works() {
        return;  // CI may lack namespace support
    }
    let server = sandboxed_server(vec![temp_dir.path().to_path_buf()]);
    let mut extra_env = HashMap::new();
    extra_env.insert("MY_VAR".to_string(), "value".to_string());
    // ... test body
}
```

### Changeset Front Matter

Package name must match `Cargo.toml`:

```yaml
---
harnx: minor
---
```

Not `default: minor` — this was a caught error during review.

## Why This Works

1. **Layered precedence**: Server's curated env (`build_child_env()`) forms base layer; per-call `env` overlays on top with last-write-wins semantics
2. **Sandbox compatibility**: `sandbox-run` already parses `--env KEY=VALUE` flags and applies them to child process, so no `sandbox-run` changes needed
3. **No isolation break**: Per-call vars don't persist in server's environment — each call starts fresh
4. **Cross-platform**: Non-sandbox path works on all platforms; sandbox path is Unix-only as expected

## Prevention Strategies

**Test Cases:**
- Non-sandbox path: `test_exec_per_call_env_vars`, `test_spawn_per_call_env_vars`
- Sandbox path: `test_sandbox_exec_per_call_env_vars`, `test_sandbox_spawn_per_call_env_vars`
- Edge cases: `test_exec_env_special_chars_and_override` (multiple `=` signs, newlines, override precedence)

**Best Practices:**
- Place `--env` flags before `--` separator in sandbox args — flags after separator go to bash
- Use `if !sandbox_runtime_works() { return; }` guard for sandbox tests
- Add `#[serde(default)]` for optional parameters to deserialize with `None`
- Use `object_schema_with_desc` for schema generation to include field descriptions

**Code Review Checklist:**
- [ ] `env` parameter added to both `ExecCommandParams` and `SpawnCommandParams`
- [ ] JsonSchema impl includes `env` with description
- [ ] Sandbox path uses `--env KEY=VALUE` format before `--` separator
- [ ] Non-sandbox path calls `command.envs()` after `build_child_env()`
- [ ] Tests cover both sandbox and non-sandbox paths
- [ ] Changeset uses correct package name (`harnx: minor`)

## Related Issues

- **GitHub Issue:** [#532 — Ability to provide environment variables as part of exec/spawn tools](https://github.com/dobesv/harnx/issues/532)
- **Related Solution:** [security-issues/environment-sanitization-bash-sandbox-2026-04-29.md](../security-issues/environment-sanitization-bash-sandbox-2026-04-29.md) — Environment sanitization pattern that created the `--env` flag mechanism
- **Related Solution:** [api-design/optional-param-append-default-2026-05-12.md](./optional-param-append-default-2026-05-12.md) — Pattern for optional serde parameters with defaults
