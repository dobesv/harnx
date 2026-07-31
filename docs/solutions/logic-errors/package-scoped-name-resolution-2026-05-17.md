---
title: "Package-scoped name resolution for compaction agents and client/model references"
date: 2026-05-17
category: logic-errors
problem_type: logic_error
component: package-namespace
root_cause: missing-package-context-resolution
resolution_type: code_fix
severity: medium
tags:
  - namespacing
  - packages
  - resolution
  - compaction-agent
  - client-config
plan_ref: harnx-package-namespacing
---

## Problem

Package agents referenced bare names (`compaction_agent: compactor`, `model: openai:gpt-4o`) that resolved globally against top-level resources, breaking package isolation. Two packages with identically-named clients or agents could collide, and agents within a package couldn't default to using resources defined in that same package.

## Symptoms

- Package agent with `compaction_agent: compactor` looked up `compactor` at top level instead of `mypkg/compactor`
- Package agent with `model: openai:gpt-4o` used global `openai` client instead of `mypkg/openai`
- Same-named clients in different packages collided on the global client list
- No way to express "use the resource defined in my package" vs "use top-level resource"

## Investigation Steps


2. Identified two resolution sites:
   - **Compaction agent**: `Config::compact_session()` reads `agent.compaction_agent()` and looks up the agent
   - **Model/client**: `apply_package_agent_transforms()` processes `model_id` and `model_fallbacks` fields

   - Bare `foo` → `pkg/foo` (package-relative)
   - `/foo` → `foo` (top-level escape, leading slash stripped)
   - `other/foo` → `other/foo` (already qualified, unchanged)

4. Implemented shared helper `resolve_package_relative_name()` in `harnx-core`.

5. Added `package: Option<String>` field to all 8 provider config structs for metadata tracking.

## Root Cause

The name resolution logic lacked awareness of package context. All bare names were resolved against the global namespace, violating the principle that packages should be self-contained.

**Compaction agent path**: `compact_session()` extracted `compaction_agent` name and directly passed it to `retrieve_agent()` without any package-relative transformation.

**Client/model path**: `apply_package_agent_transforms()` did not rewrite the client-name portion of model references, so `openai:gpt-4o` would match a top-level `openai` client instead of a package-local one.

## Solution

### Core Resolution Helper

Added `resolve_package_relative_name()` in `crates/harnx-core/src/package_namespace.rs`:

```rust
pub fn resolve_package_relative_name(name: &str, pkg_context: Option<&str>) -> String {
    if name.starts_with('/') {
        // Leading slash escapes to top-level
        name[1..].to_string()
    } else if name.contains('/') {
        // Already qualified, pass through
        name.to_string()
    } else if let Some(pkg) = pkg_context {
        // Bare name in package context → qualify with package
        format!("{pkg}/{name}")
    } else {
        // Bare name outside package context → unchanged
        name.to_string()
    }
}
```

### Compaction Agent Resolution (call-time)

In `crates/harnx-runtime/src/config/mod.rs`, modified `compact_session()`:

```rust
let active_agent_name = config.read().extract_agent().name().to_string();
let active_pkg = harnx_core::package_namespace::pkg_from_qualified(&active_agent_name);
let compaction_agent_name = config.read().extract_agent().compaction_agent().map(str::to_owned);

if let Some(name) = compaction_agent_name {
    let resolved_name = harnx_core::package_namespace::resolve_package_relative_name(&name, active_pkg);
    match config.read().retrieve_agent(&resolved_name) {
        // ...
    }
}
```

### Client/Model Resolution (load-time)

In `crates/harnx-runtime/src/config/agent.rs`, `apply_package_agent_transforms()` rewrites model references:

```rust
if let Some(model_id) = config.model_id().map(ToOwned::to_owned) {
    let resolved = match model_id.split_once(':') {
        Some((client_part, model_part)) => {
            let resolved_client = resolve_package_relative_name(client_part, Some(pkg_name));
            format!("{resolved_client}:{model_part}")
        }
        None => resolve_package_relative_name(&model_id, Some(pkg_name)),
    };
    config.set_model_id(Some(resolved));
}
```

### Client Name Qualification

In `Config::load_package_clients()`, clients are renamed from bare `openai` to `mypkg/openai`:

```rust
for client in &mut clients {
    let bare_name = client.effective_name().to_string();
    if !bare_name.contains('/') {
        let qualified = format!("{pkg_name}/{bare_name}");
        client.set_name_and_package(qualified, pkg_name.to_string());
    }
}
```

## Why This Works

- **Call-time vs load-time**: Compaction agent resolution happens at call-time because the active agent context isn't known until runtime. Client/model resolution happens at load-time because the package context is static once the agent is loaded.

- **Shared helper**: Both paths use `resolve_package_relative_name()`, ensuring consistent semantics.

- **Qualified names flow through existing caches**: Once a client is renamed to `mypkg/openai`, it works naturally with `OnceLock`-based global registries (`list_client_names()`, `list_all_models()`) without code changes.

- **Escape syntax**: The leading-slash `/name` pattern provides an explicit way to reference top-level resources when needed.

## Key Patterns Discovered

### OnceLock Caching Hazard

`list_client_names()` and `list_all_models()` in `harnx-client/src/macros.rs` use static `OnceLock` caches:

```rust
static ALL_CLIENT_NAMES: std::sync::OnceLock<Vec<String>> = OnceLock::new();
static ALL_MODELS: std::sync::OnceLock<Vec<Model>> = OnceLock::new();
```

These caches populate on first call and never reset. Tests that need fresh client state must NOT use these functions — instead test directly on `Config.clients` vector:

```rust
assert!(
    config.clients.iter().any(|client| client.effective_name() == "mypkg/openai"),
);
```

### serde(skip) + jaq Patching

Fields with `#[serde(skip)]` are reset to their `Default` value when config structs are round-tripped through `serde_json` during jaq patching (`apply_client_patch`). The `package` field must be set **after** patching, not before:

```rust
// Patching happens first
for client in &mut clients {
    apply_client_patch(client, &patch.clients);
}
// Qualification (which sets package) happens AFTER patching
for client in &mut clients {
    client.set_name_and_package(qualified, pkg_name.to_string());
}
```

### Macro-generated Enum + Manual impl

`ClientConfig` enum is generated by `register_client!` macro. To add methods that dispatch across all variants, add an explicit `impl ClientConfig` block after the macro invocation:

```rust
register_client!(OpenAIConfig, ...);

impl ClientConfig {
    pub fn set_name_and_package(&mut self, name: String, package: String) {
        match self {
            ClientConfig::OpenAIConfig(c) => { c.name = Some(name); c.package = Some(package); }
            // ... other variants
        }
    }
}
```

### Provider Config Struct Dispersion

Each LLM provider (`openai.rs`, `claude.rs`, etc.) has its own struct in `crates/harnx-core/src/provider_config/`. Adding a field to all 8 requires touching all 8 files, plus any test initializers that construct them manually.

## Prevention Strategies

**Test Cases:**
- Add integration tests for package-scoped resolution in `tests/package_loading.rs`
- Test helper function `resolve_package_relative_name()` with all edge cases
- Test that `compact_session()` resolves compaction agent within package
- Test that package agent model references are rewritten correctly
- Avoid `list_client_names()` / `list_all_models()` in tests — assert on `config.clients` directly

**Best Practices:**
- Use shared `resolve_package_relative_name()` helper for any new package-relative resolution
- Set `#[serde(skip)]` fields after jaq patching, not before
- Qualify package resources with `pkg/` prefix at load time to work with `OnceLock` caches

**Code Review Checklist:**
- [ ] Does the new entity reference need package-relative resolution?
- [ ] Is resolution happening at the right time (load-time vs call-time)?
- [ ] Are tests asserting on the right data (avoiding OnceLock caches)?

## Related Issues

- **GitHub Issue:** [#585](https://github.com/dobesv/harnx/issues/585) — Agents in a package should use models/clients defined in that package by default
- **GitHub Issue:** [#586](https://github.com/dobesv/harnx/issues/586) — Compaction agent references in a packaged agent should resolve within the package by default
- **Changeset:** `.changesets/package-scoped-resolution.md` — Breaking change documentation with migration guide
