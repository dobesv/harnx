---
title: "Migrated YAML DSL patch system to jaq/jq expression strings"
date: 2026-05-16
category: "integration-issues"
problem_type: integration_issue
component: "patch-layer"
root_cause: "legacy regex-keyed DSL replaced with standard jq expression language"
resolution_type: code_fix
severity: high
tags:
  - jaq
  - jq
  - patching
  - serde
  - json-round-trip
  - yaml-migration
  - client-config
  - package-patching
plan_ref: "jaq-patch-dsl"
last_updated: 2026-05-16
---

## Problem

The harnx workspace used a custom regex-keyed YAML DSL for applying patches to configurations and request payloads. This DSL required maintaining specialized parsing logic, interpolation of `$HARNX_MODEL`/`$HARNX_CLIENT` variables, and a `json-patch` dependency. The system was difficult to extend and required callers to understand a bespoke syntax.

## Symptoms

- Patch definitions in `models.yaml` used `patch:` blocks with regex-keyed field paths
- `RequestPatch`/`AgentPatch`/`McpServerPatch` typed structs with specialized field mutation logic
- `patch_request_data` used regex interpolation for `$HARNX_MODEL` and `$HARNX_CLIENT`
- `apply_patch` and `interpolate_patch_vars` functions required maintenance
- `HARNX_PATCH_{CLIENT}_{API}` environment variables expected JSON objects, not expression arrays

## Investigation Steps

1. Analyzed jaq v3 API patterns for serde_json evaluation
2. Created `harnx_core::jaq` module with `eval_filter` / `eval_filters` primitives
3. Identified all patch surfaces: model-level, client config, package-level, environment variables
4. Migrated 25 entries in `models.yaml` from `patch:` to `patches:` with jq expressions
5. Encountered `McpServerConfig.package` data loss during serde round-trip (field marked `#[serde(skip)]`)

## Root Cause

1. **Custom DSL complexity**: Regex-keyed patches required specialized parsing and interpolation logic that duplicated capabilities available in standard jq expressions.

2. **serde(skip) fields lost on round-trip**: `McpServerConfig.package` field marked `#[serde(skip)]` was silently dropped during serialize→patch→deserialize cycle in `apply_mcp_server_patch`.

3. **AgentConfig same vulnerability**: `apply_agent_patch` still vulnerable to same issue for `shared_variables`, `session_variables`, `tools`, `model` fields (safe at current call sites because patching happens before population).

## Solution

### Core jaq Evaluation Module

Created `harnx-core::jaq` module:

```rust
use jaq_core::load::{Arena, File, Loader};
use jaq_core::{data, unwrap_valr, Compiler, Ctx, Vars};
use jaq_json::Val;

type JsonFilter = jaq_core::Filter<data::JustLut<Val>>;

fn compile_filter(expr: &str) -> Result<JsonFilter, String> {
    let arena = Arena::default();
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let loader = Loader::new(defs);
    let modules = loader
        .load(&arena, File { code: expr, path: () })
        .map_err(|err| format!("{err:?}"))?;
    let funs = jaq_core::funs::<data::JustLut<Val>>()?;
    jaq_core::Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|err| format!("{err:?}"))
}

pub fn eval_filter(expr: &str, input: Value) -> Option<Value> {
    let filter = match compile_filter(expr) {
        Ok(filter) => filter,
        Err(error) => {
            warn!("jaq parse/compile failed for {expr:?}: {error}");
            return None;
        }
    };
    run_filter(&filter, input)
}

pub fn eval_filters(exprs: &[String], input: Value) -> Value {
    exprs.iter().fold(input, |current, expr| {
        eval_filter(expr, current.clone()).unwrap_or(current)
    })
}
```

### Preserve serde(skip) Fields

Fixed `apply_mcp_server_patch` to save/restore skipped fields:

```rust
fn apply_mcp_server_patch(server: &mut McpServerConfig, patches: &[String]) {
    if patches.is_empty() {
        return;
    }
    // Save fields marked #[serde(skip)] that are not included in JSON serialization.
    let saved_package = server.package.clone();
    let input = match serde_json::to_value(&*server) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("Failed to serialize McpServerConfig for jaq patch: {e}");
            return;
        }
    };
    let output = harnx_core::jaq::eval_filters(patches, input);
    match serde_json::from_value(output) {
        Ok(patched) => {
            *server = patched;
            // Restore #[serde(skip)] fields that are not preserved by JSON round-trip.
            server.package = saved_package;
        }
        Err(e) => log::warn!("Failed to deserialize McpServerConfig after jaq patch: {e}"),
    }
}
```

### Schema Changes

Changed patch fields from `Option<IndexMap<String, Value>>` to `Vec<String>`:
- `RequestPatch` → `RequestPatches(Vec<String>)`
- `PackagePatch.agents`, `PackagePatch.mcp_servers` → `Vec<String>`
- `ModelData.patch` → `ModelData.patches`
- `HARNX_PATCH_*` env var format: JSON array of jq expression strings

### Example Patch Migration

Before (YAML DSL):
```yaml
patch:
  body.temperature: null
  body.thinking: '{"type":"enabled","budget_tokens":16000}'
```

After (jq expressions):
```yaml
patches:
  - del(.body.temperature) | del(.body.top_p) | .body.thinking = {"type":"enabled","budget_tokens":16000}
```

## Why This Works

1. **Standard jq syntax**: Users leverage familiar jq expressions instead of learning a custom DSL.

2. **Graceful degradation**: `eval_filters` logs warnings and continues with unmodified input on parse/runtime errors, preventing single typo from breaking entire patch pipeline.

3. **Explicit save/restore**: Pattern for `#[serde(skip)]` fields ensures data loss is prevented during round-trip serialization.

4. **Filter chaining**: `eval_filters` feeds output of expression N as input to expression N+1, enabling complex transformations.

## Prevention Strategies

### Test Cases

- Add regression test for serde skip field preservation in patch round-trips
- Add test for `HARNX_PATCH_*` environment variable parsing as JSON array
- Add edge-case tests for multi-value jaq outputs and non-object returns

### Best Practices

- Always save/restore `#[serde(skip)]` fields before/after serde round-trips
- Use `eval_filters` for applying multiple patches in sequence
- For conditional patching, use jq `if-then-else` expressions with `.name` field matching
- Document that jaq expressions must come from trusted sources (no timeout enforcement)

### Code Review Checklist

- [ ] Are all `#[serde(skip)]` fields preserved across serialize→patch→deserialize cycles?
- [ ] Does patch application follow graceful degradation (warn + skip on error)?
- [ ] Are environment variable patches parsed with proper error context?
- [ ] Are model patches tested for correct jq syntax before bundling?

## Related Issues

- **GitHub:** #565 — Replace YAML DSL patches with jq/jaq expressions
- **GitHub:** #581 — Enable client config patching from packages (`PackagePatch.clients`)
- **Plan:** `jaq-patch-dsl`
- **Plan:** `patch-clients-from-packages`
- **Changeset:** `.changesets/jaq-patch-expressions.md`
- **Migration targets:** `models.yaml` (25 entries), `example_config/clients/*.yaml`, `packages/*/clients/*.yaml`

---

## Addendum: Client Config Patching (#581)

### Problem Extension

`PackagePatch.clients` existed in the schema as "dead surface" — the field was defined but never applied at runtime. Package authors could not ship `clients/` subdirectories with configs that get patched via `<pkg>.patches.yaml`.

### Key Insight: Serialize Must Be Derived Alongside Deserialize

Provider config structs (`OpenAIConfig`, `ClaudeConfig`, etc.) and `ExtraConfig` only derived `Deserialize`. The jaq patch system serializes configs to JSON, applies jq filters, then deserializes back. `Serialize` had to be added to:
- All provider config structs in `harnx-client`
- `ExtraConfig` struct
- The `ClientConfig` enum in the `register_client!` macro

Without `Serialize`, `serde_json::to_value(&client)` fails at runtime.

### No skip Fields in ClientConfig

Unlike `McpServerConfig` which has `package: Option<String>` marked `#[serde(skip)]`, `ClientConfig` has no skip fields. This means `apply_client_patch`:

```rust
fn apply_client_patch(client: &mut ClientConfig, patches: &[String]) {
    if patches.is_empty() { return; }
    let input = match serde_json::to_value(&*client) {
        Ok(v) => v,
        Err(e) => { log::warn!("Failed to serialize ClientConfig for jaq patch: {e}"); return; }
    };
    let output = harnx_core::jaq::eval_filters(patches, input);
    match serde_json::from_value(output) {
        Ok(patched) => { *client = patched; }
        Err(e) => log::warn!("Failed to deserialize ClientConfig after jaq patch: {e}"),
    }
}
```

No save/restore logic needed — JSON round-trip is lossless for `ClientConfig`.

### Package Loading Integration


```rust
let pkg_clients_dir = pkg_path.join(paths::CLIENTS_DIR_NAME);
if pkg_clients_dir.is_dir() {
    let patch = load_package_mcp_patch(pkg_name);  // loads full PackagePatch
    for mut client in Self::load_clients_from_dir(&pkg_clients_dir).unwrap_or_default() {
        if let Some(patch) = &patch {
            apply_client_patch(&mut client, &patch.clients);
        }
        config.clients.push(client);
    }
}
```

Note: `load_package_mcp_patch()` loads the full `PackagePatch` (not just MCP). The misleading name is a known non-blocker.

### Design Gap: ClientConfig Lacks Package Attribution


### Tests

`client_patch_tests` module verifies:
- Identity expression leaves config unchanged
- Empty patches no-op
- Field setting via jq (e.g., `.api_key = "sk-patched"`)
- Invalid jq expressions fail gracefully (log warning, config unchanged)
