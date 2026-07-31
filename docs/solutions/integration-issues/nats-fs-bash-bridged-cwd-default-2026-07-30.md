---
title: "MCP→NATS fs/bash Migration via Config-Based Path Bounding"
date: 2026-07-30
category: "integration-issues"
problem_type: integration_issue
component: "harnx-mcp-fs/harnx-mcp-bash/harnx-mcp-bridge"
root_cause: "MCP roots protocol deprecated; bridged servers had no path-bounding mechanism"
resolution_type: code_fix
severity: medium
tags:
  - nats
  - tool-servers
  - mcp
  - bridge
  - roots
  - sandbox
  - hooks
  - env-propagation
plan_ref: "nats-fs-bash-migration"
---

# Solution: MCP→NATS fs/bash Migration via Config-Based Path Bounding

## Problem

fs and bash MCP servers bound filesystem operations to MCP "roots". The NATS toolset protocol has no roots concept. A naively-bridged server inherits no roots (the NATS client never advertises the rmcp roots capability) and hard-denies all filesystem access. The originally-planned S3/S4 would have added a roots concept to the NATS protocol, but MCP roots is deprecated (spec 2026-07-28) — guidance is config-based path bounding, not protocol extension.

Additionally, bash's GitHub/Atlassian auth-injection hook couldn't migrate: `ToolServerConfig` has no `hooks:` field. The hook regression required a way to apply hooks on the tool-server path without schema changes.

## Symptoms

- Bridged fs/bash (via `harnx-mcp-bridge`) returned "No roots configured — all filesystem access is denied" for any path operation
- Auth headers for github.com/api.github.com and *.atlassian.com no longer injected when bash ran bridged
- `harnx-proxy-auth` failed startup validation because `$HARNX_PACKAGE_DIR/hooks/jira-auth-hook.py` resolved to `~/.config/harnx/hooks/...` (nonexistent) instead of the package directory

## Investigation Steps

Verified the empty-roots behavior: `validate_path`/`validate_write_path` in `crates/harnx-mcp/src/safety.rs` and `resolve_working_dir` in `crates/harnx-mcp-bash/src/server/lifecycle.rs` hard-deny when roots is empty. No cwd fallback.

Traced the full spawn chain: `LocalWorkerSupervisor::spawn_worker` → `ToolServerSupervisor::spawn_tool_server` → bridge `spawn_child`. None calls `.current_dir()`, so a bridged server's CWD equals the user's repo root throughout.

Investigated the hooks-proxy wrap for bash auth injection. Found `hooks-proxy` hardcoded `HookConfig.package_dir = None` (`crates/harnx-mcp-hooks-proxy/src/cli.rs:122`), and `base_hook_command` (`crates/harnx-hooks/src/executor.rs:39-46`) ALWAYS does `.env(HARNX_PACKAGE_DIR_ENV, package_dir.unwrap_or_else(config_dir))`. The explicit `.env()` overwrote the ambient `HARNX_PACKAGE_DIR` that `tool_supervisor.rs:204` exported, clobbering the correct value with config_dir().

Byte-comparison of the migrated YAML config passed verification while the auth chain was broken — because the bug was in runtime env propagation, not config text. Only an e2e smoke test spawning bridge→hooks-proxy→bash and observing `$HARNX_PACKAGE_DIR` inside the hook caught it.

## Root Cause

Two root causes:

1. **No path bounding on tool-server path**: The MCP manager injects CWD as a root for stdio servers, but `tool_servers/` never had this mechanism. The NATS client doesn't advertise the roots capability, leaving bridged servers with empty roots.

2. **Env-var overwrite in wrapper chain**: An ambient `HARNX_PACKAGE_DIR` was set by `tool_supervisor.rs`, but `hooks-proxy` passed `package_dir=None` to `base_hook_command`, which unconditionally re-set the var. Downstream components overriding env vars they don't own breaks inheritance-based config.

## Solution

### 1. Config-Based Path Bounding via `--default-root-cwd`

Added `--default-root-cwd` flag to `harnx-mcp-fs` and `harnx-mcp-bash`. When the flag is set AND roots end up empty after init, seed one root from canonicalized CWD — **unless** CWD is `$HOME` or an ancestor (deny with warning).

Key subtlety: "no usable roots" must be handled UNIFORMLY across:
- (a) Peer doesn't advertise the rmcp roots capability (the real bridged path — the bridge's NATS client never advertises it)
- (b) Peer advertises roots but returns an EMPTY list

Both cases fall through to the cwd default. Empty-roots-without-the-flag still hard-denies (unchanged behavior for direct stdio invocation).

Shared helper centralized in lower crate:

```rust
// crates/harnx-mcp/src/safety.rs
pub fn default_root_from_cwd() -> Option<PathBuf> {
    std::env::var_os("HOME")?;
    let cwd = std::env::current_dir().ok()?.canonicalize().ok()?;
    #[cfg(unix)]
    if path_is_home_or_ancestor(&cwd) {
        return None;
    }
    Some(cwd)
}

pub fn path_is_home_or_ancestor(path: &Path) -> bool {
    // Canonicalized comparison: home.starts_with(candidate)
    // Returns false when $HOME unset
}
```

`harnx-runtime` depends on `harnx-mcp`, so the guard moved DOWN to `harnx-mcp::safety` to avoid duplication (original was in `harnx-runtime/src/config/mod.rs:519`).

### 2. Wrapper Composition for Hooks

Restored bash's auth-injection hook WITHOUT schema change by nesting two existing arg-driven stdio wrappers:

```yaml
command: harnx-mcp-bridge
args:
  - --name
  - bash
  - --
  - harnx-mcp-hooks-proxy
  - --pre-tool-use
  - claude-command-persistent
  - --matcher
  - exec|spawn
  - harnx-proxy-auth
  - $temp_file_root
  - ;
  - --
  - harnx-mcp-bash
  - --default-root-cwd
```

Both wrappers spawn an arbitrary child after `--`. hooks-proxy applies the PreToolUse hook then wraps bash. General lesson: an arg-driven stdio proxy composes with the NATS bridge with no code change.

### 3. Ambient Env Propagation Fix

`hooks-proxy` now defaults `package_dir` from the INHERITED `HARNX_PACKAGE_DIR` env var when set, else `None`:

```rust
// crates/harnx-mcp-hooks-proxy/src/cli.rs:42
let package_dir = std::env::var_os(HARNX_PACKAGE_DIR_ENV).map(PathBuf::from);
```

This re-exports the correct value instead of clobbering it.

`tool_supervisor.rs:204` exports `HARNX_PACKAGE_DIR` AMBIENTLY before `.envs(&server.env)` so yaml can override if needed:

```rust
.env(HARNX_PACKAGE_DIR_ENV, tool_server_package_dir(server))
.envs(&server.env)
```

### 4. The `sh -c <script>` Quoting Technique

hooks-proxy shell-quotes every argv token (single-quotes), and the persistent hook runs via `sh -c`. To keep `$HARNX_PACKAGE_DIR` expanding at RUNTIME while `$temp_file_root` stays literal (proxy-auth substitutes it from request vars), pass the proxy-auth command as a single `sh -c '<verbatim old command>'` token — nested sh re-expands.

## Why This Works

- **Config-based roots** aligns with MCP spec direction (roots protocol deprecated) and reproduces the old "CWD + config" behavior without protocol change
- **Wrapper composition** restores features the `ToolServerConfig` schema lacks without extending the schema
- **Env inheritance** now works correctly: `tool_supervisor` sets the ambient value, wrappers inherit and re-export it, yaml can override if needed

## Prevention Strategies

**Test Cases:**
- Add e2e test spawning bridge→hooks-proxy→bash that asserts `$HARNX_PACKAGE_DIR` resolves to the package dir inside the hook (not config_dir)
- Test `--default-root-cwd` behavior: repo-cwd seeds root, `$HOME`-cwd denies with warning, flag-absent denies
- Test both empty-roots paths: (a) no capability advertised, (b) capability advertised but empty list

**Best Practices:**
- When a downstream component unconditionally sets an env var, ensure it derives the value from inheritance OR document that inheritance is intentionally clobbered
- For wrapper-composition + env-propagation, config-fidelity checks are necessary but insufficient; add e2e tests observing values at leaves
- Centralize guard logic in lower crates to avoid duplication across crate boundaries

**Code Review Checklist:**
- [ ] Does the wrapper inherit or override ambient env vars? If override, is the source documented?
- [ ] Are "no usable roots" cases handled uniformly (no-capability AND empty-list)?
- [ ] Is path-bounding logic centralized, not duplicated across crates?

## Related Issues

- **Issue:** [#1224](https://github.com/dobesv/harnx/issues/1224) — MCP→NATS migration
- **Related Solution (S1):** [nats-mcp-bridge-2026-07-30.md](./nats-mcp-bridge-2026-07-30.md) — Generic bridge design
- **Related Solution (S2):** [nats-tool-servers-config-driven-bootstrap-2026-07-29.md](./nats-tool-servers-config-driven-bootstrap-2026-07-29.md) — Config-driven tool server bootstrap
- **Related Solution:** [sandbox-home-exposure-ancestor-walk-2026-05-21.md](../security-issues/sandbox-home-exposure-ancestor-walk-2026-05-21.md) — $HOME-ancestor guard pattern

## Follow-ons

- example_config `rename_tools` and `roots: ["$HOME"]` dropped (not on tool_server path)
- Host-side CWD root injection in `servers_split.rs` KEPT — user `mcp_servers/` stdio path + `--mcp-root` still depend on it
