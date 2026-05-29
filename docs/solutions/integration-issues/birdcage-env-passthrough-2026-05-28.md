---
title: "Birdcage Environment Variable Passthrough for Interactive Sandboxes"
date: 2026-05-28
category: integration-issues
problem_type: integration_issue
component: harnx-sandbox-run
root_cause: "Birdcage Exception::Environment only allows explicitly whitelisted vars; default environment wiped without explicit passthrough list"
resolution_type: code_fix
severity: high
tags:
  - sandboxing
  - birdcage
  - environment-variables
  - interactive-shell
  - security
plan_ref: harnx-sandbox-run
---

## Problem

Birdcage sandbox starts with empty environment unless variables are explicitly added via `Exception::Environment`. A standalone sandbox wrapper that doesn't inherit baseline `PATH`, `HOME`, `TERM`, etc. breaks interactive shell use.

## Symptoms

- `harnx-sandbox-run -- bash` fails with "bash: command not found" (no `PATH`)
- Shell startup files fail to find `HOME`
- Terminal features missing (no `TERM`)
- XDG tools cannot locate config directories

## Investigation Steps

1. Tested `harnx-sandbox-run -- env` — output was empty except explicit `--env` vars
2. Reviewed `harnx-mcp-bash/server.rs` — uses `DEFAULT_ENV_ALLOWLIST` for baseline vars
3. Checked birdcage docs — `Exception::Environment` is whitelist, not passthrough
4. Compared MCP server (curated env) vs standalone runner (intended parity)

## Root Cause

Birdcage's `restrict_env_variables()` blocks all environment variables except those explicitly added:

```rust
// In birdcage internals:
fn restrict_env_variables() {
    for (key, _) in std::env::vars() {
        if !allowed.contains(&key) {
            std::env::remove_var(&key);
        }
    }
}
```

The `harnx-sandbox-run` initial implementation added only:
1. CLI `--env KEY=VALUE` args
2. Hook-provided env mutations

No baseline vars (`HOME`, `PATH`, `TERM`) were inherited, breaking shell ergonomics.

## Solution

Define a default passthrough list and apply before CLI/hook overrides:

```rust
#[cfg(unix)]
const DEFAULT_ENV_PASSTHROUGH: &[&str] = &[
    "HOME", "USER", "LOGNAME", "SHELL", "TERM",
    "LANG", "LC_ALL", "LC_CTYPE", "PATH", "TMPDIR",
];

// Apply baseline env
let mut all_env: Vec<(String, String)> = Vec::new();

for name in DEFAULT_ENV_PASSTHROUGH {
    if let Ok(value) = env::var(name) {
        all_env.push((name.to_string(), value));
    }
}

// XDG_* convention: passthrough all XDG-prefixed vars
for (name, value) in env::vars() {
    if name.starts_with("XDG_") && !all_env.iter().any(|(k, _)| k == &name) {
        all_env.push((name, value));
    }
}

// CLI --env (overwrites baseline)
for raw in &cli.env_vars {
    // parse KEY=VALUE and push
}

// Hook-provided env (highest precedence)
for (key, value) in hook_env {
    all_env.push((key, value));
}

// Apply to sandbox
for (key, value) in all_env {
    sandbox.add_exception(Exception::Environment(key.into()))?;
    env::set_var(&key, &value);
}
```

**Precedence order** (low to high):
1. `DEFAULT_ENV_PASSTHROUGH` baseline
2. `XDG_*` automatic passthrough
3. CLI `--env KEY=VALUE`
4. Hook-injected env

## Why This Works

1. **Shell usability**: `PATH` allows finding commands, `HOME` allows `~` expansion, `TERM` enables colors/paging
2. **Tool compatibility**: XDG vars (`XDG_CONFIG_HOME`, `XDG_DATA_HOME`) are standard for Linux desktop tools
3. **Security maintained**: Explicit list prevents leaking secrets like `AWS_SECRET_ACCESS_KEY`
4. **Override capability**: CLI and hooks can still override any value (precedence)

This mirrors the `DEFAULT_ENV_ALLOWLIST` in `harnx-mcp-bash` while keeping separate constants (standalone use has slightly different ergonomics).

## Prevention Strategies

**Test Cases:**
- Assert `PATH`, `HOME`, `TERM` present in sandboxed child
- Assert `XDG_*` vars passed through
- Assert CLI `--env` overwrites baseline
- Assert hook env overwrites CLI and baseline

**Best Practices:**
- Always define baseline env passthrough for interactive sandboxes
- Document precedence order explicitly
- Test both with and without hooks (different code paths)
- Use `Exception::Environment` + `set_var` before `spawn()` — birdcage inspects current process env

**Code Review Checklist:**
- [ ] Is baseline env defined and applied before sandbox spawn?
- [ ] Are XDG vars handled?
- [ ] Is precedence documented and tested?
- [ ] Are secrets excluded from default list?

## Related Issues

- **GitHub Issue:** [#575 — Standalone CLI Sandbox Wrapper](https://github.com/dobesv/harnx/issues/575)
- **Plan:** harnx-sandbox-run
- **Related Solution:** [../security-issues/environment-sanitization-bash-sandbox-2026-04-29.md](../security-issues/environment-sanitization-bash-sandbox-2026-04-29.md) — Full environment curation pattern
- **Related Solution:** [per-call-env-param-bash-mcp-2026-05-13.md](per-call-env-param-bash-mcp-2026-05-13.md) — Per-call env override pattern
- **Related Solution:** [../security-issues/sandbox-home-exposure-ancestor-walk-2026-05-21.md](../security-issues/sandbox-home-exposure-ancestor-walk-2026-05-21.md) — Path exception safety
