---
title: "ACP server extraction + package delegation #804 root-cause fix"
date: 2026-06-11
category: logic-errors
problem_type: logic_error
component: harnx-runtime
root_cause: "name field conflated spawn-target and display-name identities"
resolution_type: code_fix
severity: high
tags:
  - acp
  - package-delegation
  - subprocess-spawn
  - display-names
  - binary-extraction
  - test-harness
plan_ref: acp-server-extraction-and-package-delegation-fix
---

## Problem

`AcpServerConfig.name` field served dual roles:
1. **Canonical SPAWN TARGET** — the package-qualified agent identifier used in subprocess args (e.g., `pantheon/aristarchus`)
2. **LLM/tool DISPLAY NAME** — the bare name shown to the LLM (e.g., `aristarchus`)

When `reinit_managers_for_agent` rewrote `s.name` to the bare display form for same-package peers, any code that derived spawn args from `name` would produce incorrect bare identifiers, breaking same-package delegation.

## Symptoms

- Same-package delegation (e.g., `pantheon/atlas` → `pantheon/aristarchus`) failed with "agent not found"
- Spawn args contained bare `aristarchus` instead of qualified `pantheon/aristarchus`
- Bug masked by same-named top-level agent (`agents/aristarchus.md`) until it was removed
- `package_loading` integration tests flaked under `cargo test` due to shared global model catalog

## Investigation Steps

1. Traced `acp_server_display_name` in `servers_split.rs:196` — rewrites `s.name` per-agent context
2. Verified `AcpManager` keys clients by rewritten `name` — correct for display
3. Found ACP client spawns `config.command` + `config.args` verbatim at `harnx-acp/src/client.rs:613-625`
4. Confirmed `auto_register_agents` already set `args = [agent_name.clone()]` with qualified names
5. Identified gap: if any code derived spawn target from `name`, display rewrite would corrupt it
6. Found package YAML configs in `packages/<pkg>/acp_servers/*.yaml` with bare args that needed qualification

## Root Cause

**Dual-identity conflation.** The `name` field held both the canonical identifier (used for dispatch) and the display name (shown to LLM). The display-rewrite cloned servers and mutated `name`, but any downstream code expecting `name` to be the spawn target would receive the wrong value.

The fix separates concerns:
- **Spawn target** lives in `args[0]` — never rewritten, always canonical
- **Display name** computed on-demand via `acp_server_display_name()` — transient, not stored

General lesson: when one field serves both a human/LLM-facing display role and a machine/spawn-target role, separate concerns so display rewrites cannot corrupt the spawn target.

## Solution

### 1. Auto-registration preserves qualified args at source

```rust
// crates/harnx-runtime/src/config/loader_split.rs:179-189
acp_servers.push(AcpServerConfig {
    name: agent_name.clone(),          // qualified: "pantheon/aristarchus"
    command: command.clone(),
    args: vec![agent_name.clone()],    // spawn target = qualified name
    package: pkg,
    ..
});
```

`list_agents()` returns qualified names for package agents, so `args[0]` carries the canonical spawn target from the start.

### 2. Display rewrite clones without touching args

```rust
// crates/harnx-runtime/src/config/servers_split.rs:191-199
let acp_servers: Vec<AcpServerConfig> = self
    .acp_servers
    .iter()
    .map(|s| {
        let mut s = s.clone();
        s.name = acp_server_display_name(&s, agent_package);  // rewrite display only
        s  // args untouched
    })
    .collect();
```

### 3. Normalize package YAML bare args

Package-loaded YAML servers with `args: [stem]` get qualified:

```rust
// crates/harnx-runtime/src/config/servers_split.rs:6-14
fn normalize_package_acp_server_args(server: &mut AcpServerConfig, pkg_name: &str) {
    let stem = server.name.as_str();
    let qualified = qualify_agent_name(pkg_name, stem);
    for arg in &mut server.args {
        if arg == stem {
            *arg = qualified.clone();
        }
    }
}
```

### 4. Sibling-binary discovery helper

```rust
// crates/harnx-runtime/src/config/loader_split.rs:282-322
fn harnx_acp_server_command() -> String {
    std::env::current_exe()
        .map(|exe| harnx_acp_server_command_from_current_exe(&exe))
        .unwrap_or_else(|_| fallback_harnx_acp_server_command())
}

fn harnx_acp_server_command_from_parent(parent_dir: Option<&Path>, is_absolute: bool) -> String {
    let Some(parent) = parent_dir else { return fallback() };
    if !is_absolute { return fallback(); }
    let sibling = parent.join(harnx_acp_server_binary_name());
    if sibling.is_file() { sibling.to_string_lossy().to_string() }
    else { fallback() }  // PATH lookup
}
```

Cross-reference: [xtask-pattern-dynamic-bin-discovery-2026-06-10.md](../workflow-issues/xtask-pattern-dynamic-bin-discovery-2026-06-10.md) — same sibling-discovery pattern.

## Why This Works

- **Source of truth:** Spawn target is `args[0]`, set once at auto-registration, never derived from `name`
- **Display isolation:** `s.name` rewrite clones `AcpServerConfig` but leaves `args` untouched
- **YAML hardening:** Bare args in package YAML configs get normalized to qualified before display rewrite
- **Binary discovery:** Sibling lookup works for workspace builds; PATH fallback for system installs

## Test-Harness Gotchas

### a) Integration tests flake under `cargo test`

`package_loading` tests share a global model catalog (`harnx-client::ALL_PROVIDER_MODELS`). Parallel threads cause "Unknown chat model 'openai:gpt-4o'" panics that vary by run.

**Fix:** Use `cargo nextest run` (per-process isolation). Documented in `AGENTS.md`.

### b) Cannot test `pub(super)` from integration tests

`reinit_managers_for_agent` is `pub(super)` — not callable from `tests/package_loading.rs`.

**Workaround:** Assert the invariant via public test re-export:
```rust
// harnx-runtime/src/lib.rs re-exports for testing:
pub fn acp_server_display_name_for_test(server: &AcpServerConfig, pkg: Option<&str>) -> String;

// Test asserts display name is bare while source args stay qualified:
assert_eq!(acp_server_display_name_for_test(original_server, Some("pantheon")), "aristarchus");
assert_eq!(original_server.args, vec!["pantheon/aristarchus"]);
```

### c) Sibling binary resolution in tmux_e2e fixtures

`harnx/tests/tmux_e2e.rs` resolves `harnx-acp-server` via `with_file_name`:

```rust
// crates/harnx/src/test_utils/interrupt.rs:23-26
pub fn harnx_acp_server_bin(harnx_bin: &Path) -> PathBuf {
    let ext = std::env::consts::EXE_SUFFIX;
    harnx_bin.with_file_name(format!("harnx-acp-server{ext}"))
}
```

No `CARGO_BIN_EXE` needed — workspace builds all members into same `target/<profile>/`.

## Prevention Strategies

**Test Cases:**
- Unit tests for sibling discovery (present vs missing sibling)
- Integration test: auto-registered ACP args stay qualified after display rewrite
- Integration test: package YAML bare args normalized to qualified
- E2E test: same-package delegation succeeds when no top-level agent masks

**Best Practices:**
- When a field serves both machine and human roles, separate concerns explicitly
- Never derive machine-identifiers from display-rewriteable fields
- Use `*_for_test` re-exports to verify invariants without testing private methods
- Integration tests with shared state: run under `cargo nextest` with process isolation

**Code Review Checklist:**
- [ ] Does `AcpServerConfig.name` rewrite touch `args`? (should not)
- [ ] Are package YAML bare args normalized before display rewrite?
- [ ] Do integration tests isolate environment via `ENV_MUTEX` and `EnvGuard`?
- [ ] Does spawn command derive target from `name` instead of `args`?

## Related Issues

- **GitHub:** [#550](https://github.com/dobesv/harnx/issues/550) — Move ACP support out of main harnx binary
- **GitHub:** [#804](https://github.com/dobesv/harnx/issues/804) — Verify pantheon handoff/delegation within a package
- **Prior Art:** [xtask-pattern-dynamic-bin-discovery-2026-06-10.md](../workflow-issues/xtask-pattern-dynamic-bin-discovery-2026-06-10.md) — sibling binary discovery pattern
- **Related:** [package-relative-agent-delegation-2026-06-09.md](./package-relative-agent-delegation-2026-06-09.md) — display-name resolution for `_session_*`
