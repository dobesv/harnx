---
title: "Serde skip fields lost during JSON round-trip for jaq/jq config patching"
date: 2026-06-12
category: "logic-errors"
problem_type: logic_error
component: "patch-layer"
root_cause: "serde(skip) fields absent from JSON and reset to Default after deserialize"
resolution_type: code_fix
severity: high
tags:
  - serde
  - jaq
  - jq
  - patching
  - json-round-trip
  - client-config
  - mcp-server-config
plan_ref: "client-name-from-filename"
---

## Problem

When a Rust struct field is marked `#[serde(skip)]` and that struct is round-tripped through `serde_json::to_value` → jaq filter → `serde_json::from_value` for config patching, the skipped field is silently lost: absent from the JSON the filter sees, and reset to its `Default::default()` after deserialization. If the skipped field holds identity or runtime-critical data, downstream logic breaks.

## Symptoms

- Patch filters that key on the skipped field (e.g. `if .name == "claude" then ...`) silently stop matching
- After patching, the field value becomes empty/None instead of its pre-patch value
- Qualified names derived from the field become invalid (e.g. `pkg/` instead of `pkg/openai`)
- Test fixtures that parse YAML directly via `serde_yaml::from_str` without going through the loader produce configs with empty identity fields

```yaml
# Example: package patch filter stops matching after name becomes #[serde(skip)]
patches:
  clients:
    - 'if .name == "claude" then .api_key = "sk-..." end'
# Result: filter never matches, .name is absent from JSON input
```

## Investigation Steps

1. Traced `apply_client_patch` in `crates/harnx-runtime/src/config/patches_split.rs` — found it serialize → jaq → deserialize without special handling for skipped fields
2. Reviewed provider config structs — confirmed `#[serde(skip)] pub name: String` on all 9 client providers
3. Compared with MCP server patch — `apply_mcp_server_patch` already saves/restores `package` (also skip), but only `package` because `McpServerConfig.name` uses `#[serde(default)]` (serialized)
4. Identified two impacts: (a) `.name` filters don't match, (b) package client names become `pkg/` after patch qualification

## Root Cause

`#[serde(skip)]` has two effects during round-trip:

1. **Serialization omits the field**: `serde_json::to_value(&config)` produces JSON without that field
2. **Deserialization uses Default**: `serde_json::from_value(json)` initializes the field via `Default::default()`

For `String` fields, this means empty string `""`. The jaq filter sees JSON without the field, and after the filter runs, the field is reset.

This is expected serde behavior, but problematic when:
- The skipped field is runtime-derived (filename stem, dynamic provider)
- The field is identity-critical (used for model resolution, env var construction)
- Downstream code assumes the field was set by the loader

## Solution

### Pattern: Save-Inject-Restore

Before the serde round-trip, save skipped fields. To enable filter matching, inject identity fields into the JSON. After `from_value`, restore the fields (preferring patched values if the filter set them).

```rust
pub(super) fn apply_client_patch(client: &mut ClientConfig, patches: &[String]) -> Result<()> {
    if patches.is_empty() {
        return Ok(());
    }

    // 1. Save skipped fields before serialization
    let saved_name = match client {
        ClientConfig::Unknown => None,
        _ => Some(client.effective_name().to_string()),
    };
    let saved_package = /* match each variant for package */;

    // 2. Serialize to JSON
    let mut input = serde_json::to_value(&*client)?;

    // 3. Inject saved name into JSON for filter visibility
    if let (Some(name), serde_json::Value::Object(ref mut obj)) = (&saved_name, &mut input) {
        obj.insert("name".to_string(), serde_json::Value::String(name.clone()));
    }

    // 4. Run jaq filters
    let output = harnx_core::jaq::eval_filters_strict(patches, input)?;

    // 5. Extract patched name if filter set one
    let patched_name = output
        .as_object()
        .and_then(|obj| obj.get("name"))
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string);

    // 6. Deserialize back
    *client = serde_json::from_value(output)?;

    // 7. Restore skipped fields (prefer patched, fallback to saved)
    if let Some(name) = patched_name.or(saved_name) {
        client.set_name(name);
    }
    client.set_package(saved_package);

    Ok(())
}
```

### Variant Differences

| Config Type | `name` attribute | `package` attribute | Fields to save/restore |
|-------------|------------------|---------------------|------------------------|
| `McpServerConfig` | `#[serde(default)]` | `#[serde(skip)]` | `package` only |
| `ClientConfig` | `#[serde(skip)]` | `#[serde(skip)]` | Both `name` and `package` |

MCP servers use `#[serde(default)]` for name, so it serializes and filters can see `.name` naturally. Clients use `#[serde(skip)]` for name (preventing YAML re-serialization), requiring explicit injection.

### Test Fixture Pattern

Any test helper that constructs configs without the file loader must explicitly set the name field:

```rust
// BEFORE (broken after serde-skip change)
let config: ClientConfig = serde_yaml::from_str(r#"
type: claude
api_key: sk-test
"#)?;

// AFTER (must set name explicitly)
let mut config: ClientConfig = serde_yaml::from_str(r#"
type: claude
api_key: sk-test
"#)?;
config.set_name("test-client".to_string());  // Or client name matching test intent
```

## Why This Works

1. **Save before serialize**: Captures runtime-derived values before they're lost
2. **Inject for filters**: Makes identity fields visible to jaq expressions, preserving documented filter semantics
3. **Restore after deserialize**: Ensures downstream code sees correctly populated fields
4. **Prefer patched values**: Allows filters to intentionally modify identity (e.g., rename a client)

The inject step is key for backward compatibility: it ensures documented patch examples like `if .name == "claude" then ...` continue working despite the field being skipped.

## Prevention Strategies

### Test Cases

- Add regression test: patch filter matching on `.name` should work
- Add regression test: qualified name preserved after patching (no `pkg/`)
- Add test: patched name trumps saved name if filter sets one
- Add test: direct YAML parse without `set_name` fails model resolution

### Best Practices

- Always save/restore `#[serde(skip)]` fields around serde round-trips
- Inject identity fields into JSON so filters can match on them
- Prefer `#[serde(skip)]` over `#[serde(default)]` when field should never serialize (prevents accidental YAML pollution)
- Document that test helpers bypassing the loader must set skipped fields explicitly

### Code Review Checklist

- [ ] All `#[serde(skip)]` fields saved before `serde_json::to_value`?
- [ ] Identity fields (name, id) injected into JSON for filter visibility?
- [ ] Fields restored after `serde_json::from_value`?
- [ ] Test helpers that parse YAML directly call `set_name`?
- [ ] Patched values preferred over saved values (allow intentional modification)?

## Related Issues

- **Prior solution:** [jaq-expression-patching-yaml-to-jq-migration-2026-05-16](../integration-issues/jaq-expression-patching-yaml-to-jq-migration-2026-05-16.md) — MCP server `package` save/restore pattern
- **Strict evaluation:** [strict-jaq-patch-evaluation-2026-05-24](../logic-errors/strict-jaq-patch-evaluation-2026-05-24.md) — error propagation for invalid patches
- **Issue:** #823 — Client name derived from filename, mirrors MCP loader pattern
