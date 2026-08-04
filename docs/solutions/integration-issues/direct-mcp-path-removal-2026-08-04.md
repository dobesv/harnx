---
title: "Direct MCP path removal — bridge-only architecture with rmcp-clean boundary"
date: 2026-08-04
category: "integration-issues"
problem_type: integration_issue
component: "harnx-mcp, harnx-mcp-bridge, harnx-toolset, harnx-runtime"
root_cause: "Dual MCP integration paths (direct McpManager + bridge) created maintenance burden; removing direct path required careful boundary preservation and prerequisite sequencing"
resolution_type: code_fix
severity: medium
tags:
  - mcp
  - nats
  - bridge
  - architecture
  - rmcp-clean-boundary
  - serde-skip
  - meta-propagation
plan_ref: "remove-direct-mcp-path"
---

## Problem

harnx had two paths to external MCP servers: (1) direct stdio via `McpManager` spawning children, and (2) NATS via `harnx-mcp-bridge`. The direct path duplicated logic, complicated the architecture, and blocked further convergence. Removing it required: relocating shared utilities out of `crates/harnx-mcp`, propagating tool `_meta` templates through the bridge path (a latent gap), and atomic crate deletion without breaking dependents.

## Symptoms

- **Architectural duplication**: `McpManager` and `harnx-mcp-bridge` both managed MCP child servers with overlapping lifecycle logic
- **Template gap**: Bridge dropped `tool._meta` at `map_tool` because `ToolSpec` had no `meta` field; only `McpClient::build_tool_declaration` extracted `call_template`/`result_template`
- **rmcp contamination risk**: Initial plan suggested moving `schema::object_schema_with_desc` to `harnx-core`, but it uses `rmcp::schemars::Schema` — would violate "harnx-core is rmcp-free" invariant
- **serde(skip) field loss**: `apply_tool_server_patch` (replacing deleted `apply_mcp_server_patch`) round-tripped through serde_json, silently dropping `ToolServerConfig.package` → broken package-scoped namespacing
- **Test-env flakiness**: Timing-sensitive 50ms timeout tests starved under full nextest parallelism; piping nextest output through `tail` hung due to leaked test-daemon FD

## Investigation Steps

1. **Audit coupling**: Verified `McpClient::build_tool_declaration` was the ONLY place `_meta.call_template`/`result_template` were extracted. `NatsToolProvider` hardcoded `None`. => removing McpManager before fixing _meta propagation would silently drop ALL template support.

2. **Trace shared utils**: `crates/harnx-mcp` exported `content::WithAudience`, `safety`, `schema` used by native toolsets. Each has different rmcp dependence: `safety` is rmcp-agnostic; `content` and `schema` use rmcp types directly.

3. **Verify bridge independence**: Bridge uses `rmcp` directly, not `harnx-mcp`. Deleting the crate wouldn't break the bridge.

4. **Check prereq ordering**: Since `_meta` templates were only read at discovery in the direct path, the #1349 propagation fix had to land BEFORE the direct-path removal. Deferring it would have broken template support for all bridged tools.

5. **Inspect existing solution docs**: Found `logic-errors/serde-skip-patch-round-trip-2026-06-12.md` documenting this exact class of bug for client/MCP configs. The `apply_tool_server_patch` case was the same pattern.

## Root Cause

The dual-path architecture violated DRY and complicated maintenance. Removing the direct path required:

1. **Prerequisite sequencing**: The `_meta` propagation fix (#1349) was a hard prerequisite folded into PR1, not deferred. The direct path contained the only code that extracted templates.

2. **rmcp-clean boundary**: `harnx-toolset` and `harnx-core` are "transport-independent protocol types" — no rmcp dependency. `ToolSpec.meta` had to be in-house `Option<serde_json::Map<String, Value>>` matching rmcp's `Meta` transparently, NOT an rmcp type.

3. **Utility relocation by dependency**: `schema::object_schema_with_desc` used `rmcp::schemars::Schema`, so moved to `harnx-toolset-server` (already has rmcp), NOT `harnx-core`. The plan's initial assumption that it was rmcp-agnostic was wrong.

4. **serde(skip) round-trip loss**: `ToolServerConfig.package` is `#[serde(skip)]`. The new `apply_tool_server_patch` serialized→patched→deserialized without saving/restoring it, breaking package-scoped namespacing.

## Solution

Executed in two PRs for safe, atomic transition.

### PR1: Prerequisites (independently mergeable)

**1. Fix release.yaml refs to deleted `harnx-acp-server`**

Simple deletion, caught early.

**2. Relocate shared utils (correction to initial plan)**

- `safety` → `harnx-core` (rmcp-agnostic: path_to_file_uri, sanitize_output_text, etc.)
- `content::WithAudience` → `harnx-toolset-server` (uses rmcp::model types)
- `schema::object_schema_with_desc` → `harnx-toolset-server` (uses rmcp::schemars::Schema)

Kept exports in `harnx-mcp` until all callers repointed, `rg`-verified zero references, then deleted the module files.

**3. Propagate `_meta` through bridge path (hard prerequisite)**

Added `ToolSpec.meta: Option<serde_json::Map<String, Value>>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`:

```rust
// crates/harnx-toolset/src/lib.rs
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub idempotent_hint: bool,
    pub read_only_hint: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Map<String, serde_json::Value>>,
}
```

Bridge extracts rmcp Meta transparently:

```rust
// crates/harnx-mcp-bridge/src/lib.rs:343-359
fn map_tool(server_name: &str, tool: Tool) -> ToolSpec {
    ToolSpec {
        name: format!("{server_name}_{}", tool.name),
        // ...
        meta: tool.meta.map(|m| m.0),  // rmcp::model::Meta is transparent Map
    }
}
```

Runtime provider extracts templates inline (no new abstractions):

```rust
// crates/harnx-runtime/src/nats_tool_provider.rs:288-299
let call_template = spec
    .meta
    .as_ref()
    .and_then(|m| m.get("call_template"))
    .and_then(|v| v.as_str())
    .map(|s| s.to_string());
let result_template = spec
    .meta
    .as_ref()
    .and_then(|m| m.get("result_template"))
    .and_then(|v| v.as_str())
    .map(|s| s.to_string());
```

### PR2: Atomic deletion

**1. Remove McpManager wiring**

Deleted from `harnx-runtime`: `McpManager`, `McpServerConfig`, `mcp_servers_dir()`, CLI `.mcp` commands, completion branches.

**2. Delete `crates/harnx-mcp` entirely**

After PR1, `rg "harnx_mcp::" crates/` returned zero matches. Deleted crate, dropped from workspace Cargo.toml, removed runtime dependency.

**3. Rewrite test fixtures**

`interrupt.rs` and `openai_responses_e2e.rs` now write `tool_servers/time.yaml` launching `harnx-mcp-bridge --name time -- harnx-mcp-time`.

**4. Fix serde(skip) loss in `apply_tool_server_patch`**

```rust
// crates/harnx-runtime/src/config/servers_split.rs:35-51
pub(super) fn apply_tool_server_patch(
    server: &mut ToolServerConfig,
    patches: &[String],
) -> Result<()> {
    if patches.is_empty() {
        return Ok(());
    }

    let saved_package = server.package.clone();  // Save before round-trip
    let input = serde_json::to_value(&*server).context("Failed to serialize tool server config")?;
    let output = harnx_core::jaq::eval_filters_strict(patches, input).context("jq patch evaluation")?;
    *server = serde_json::from_value(output).context("Failed to deserialize patched tool server")?;
    server.package = saved_package;  // Restore after round-trip
    Ok(())
}
```

Added regression test asserting package preserved after patching.

**5. Add worker `use_tools` fallback test**

Verified `configured_worker_services` falls back to `ConfigData.use_tools` when worker has no active Agent. Added test + inline comment.

## Why This Works

1. **Two-PR decomposition**: PR1 leaves `harnx-mcp` intact but unused by toolsets. PR2 is pure deletion. Each independently builds/tests green.

2. **rmcp-clean boundary**: `harnx-toolset` remains rmcp-free. Adapters (bridge, toolset-server) translate between rmcp types and in-house wire types. `ToolSpec.meta` is pure JSON, transportable over NATS without rmcp.

3. **Prerequisite sequencing**: The `_meta` propagation fix landed before the direct path was removed. Template support survived the transition.

4. **No half-migration**: Kept `harnx-mcp` exports until all callers repointed, `rg`-verified zero references, then deleted atomically. Deletes in a single commit, not scattered across PRs.

5. **serde(skip) preservation**: Same pattern as `apply_client_patch` and the documented pattern in `serde-skip-patch-round-trip-2026-06-12.md`. Save before serialize, restore after deserialize.

## Prevention Strategies

### Test Cases

- **meta round-trip**: Bridge unit test asserts child tool `_meta.call_template` appears in resulting `ToolSpec.meta`
- **runtime extraction**: Provider unit test asserts `ToolSpec` with `meta.call_template`/`result_template` populates `ToolDeclaration`
- **package preservation**: `apply_tool_server_patch` test asserts package field preserved after jq patch
- **worker fallback**: Test asserting `configured_worker_services` uses `ConfigData.use_tools` when Agent is None
- **rmcp-free invariant**: `cargo tree -p harnx-toolset | rg rmcp` returns empty

### Code Review Checklist

- [ ] When removing a crate, are ALL consumers repointed BEFORE deletion?
- [ ] When relocating code, check its dependencies — rmcp-using code goes to rmcp-aware crates
- [ ] When serializing→patching→deserializing, are `#[serde(skip)]` fields saved/restored?
- [ ] When removing a code path, is there unique functionality that must migrate first?

### Best Practices

- **Prerequisite analysis**: If a code path contains unique extraction logic, that logic must migrate BEFORE the path is deleted
- **Dependency-aware relocation**: Trace imports before moving code; rmcp-using code cannot go to rmcp-free crates
- **Atomic deletion**: Keep exports until zero references, then delete in one commit
- **serde(skip) hygiene**: Any function that serializes→deserializes a struct with `#[serde(skip)]` fields MUST save/restore them
- **Test-env tuning**: Under high parallelism, timing-sensitive tests may starve. Run `cargo nextest --workspace -j 4` on loaded machines.

### Pitfall Detection

- **Prereq ordering**: Before deleting a path, `rg` for unique functionality. If only one path extracts something, that extraction must migrate first.
- **Serde round-trips**: Search for functions that call `serde_json::to_value` and `from_value` on the same struct. Check for `#[serde(skip)]` fields.
- **Test flakes**: Tests with <100ms timeouts are fragile under load. Consider generous timeouts or skip under stress mode.

## Related Issues

- **Issue**: [#1224](https://github.com/dobesv/harnx/issues/1224) — Remove direct MCP path (final slice)
- **Issue**: [#1349](https://github.com/dobesv/harnx/issues/1349) — Propagate tool _meta over bridge (prerequisite)
- **Prior Art**: [serde-skip-patch-round-trip-2026-06-12.md](../logic-errors/serde-skip-patch-round-trip-2026-06-12.md) — Same serde(skip) bug class for client configs
- **Prior Art**: [shared-allowlist-harmonization-shebang-fix-2026-08-03.md](../security-issues/shared-allowlist-harmonization-shebang-fix-2026-08-03.md) — "No half-migration" atomic deletion pattern
- **Prior Art**: [stateful-toolset-conversion-2026-08-02.md](./stateful-toolset-conversion-2026-08-02.md) — ToolSpec vs MCP Tool.meta gap analysis
- **Prior Art**: [nats-mcp-bridge-2026-07-30.md](../nats-mcp-bridge-2026-07-30.md) — Bridge architecture foundation
