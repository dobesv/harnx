---
title: "NATS Tool Discovery Left Native Tools Unwired From LLM Schema"
date: 2026-08-04
category: "integration-issues"
problem_type: integration_issue
component: "harnx-runtime, NatsToolProvider, agent_loop, tool_supervisor"
root_cause: "native-NATS conversion wired registration but not schema discovery; Config.tools cleared at load; NatsToolProvider only consulted post-tool-call"
resolution_type: code_fix
severity: critical
tags:
  - nats
  - tool-discovery
  - schema
  - e2e-test
  - test-coverage
  - schemars
plan_ref: "restore-package-tool-naming"
---

## Problem

Native-NATS tool conversion left tool discovery unwired. Native tool declarations (fs, bash, plans, grep, time) never reached the LLM function schema — `NatsToolProvider` was only consulted after the model emitted a tool call, and `Config.tools` was cleared at load. Native tools also registered raw names (`read`), so prefixed `use_tools:[fs_read]` matched nothing. No test covered the full model-schema→routing→execution path, which is how the regression shipped.

## Symptoms

- Agent requests with `use_tools: [fs_read]` produced empty tool schema in completions
- Native tools registered with raw names but routing expected prefixed names
- Tool declarations visible in KV registry but not in LLM function-call schema
- Existing tests passed because they used raw names + bare `*` selectors + injected calls, routing around the gap
- Production agents could not invoke native tools by their documented package-aware names

## Investigation Steps

1. Traced `NatsToolProvider::discover` — found it builds registrations but `Config::select_tools` never called it
2. Checked `Config::load_from_file` — `self.tools` cleared by design for NATS mode
3. Located the schema path: `prepare_completion_data` → `Config::tool_declarations_for_use_tools` → static `self.tools.declarations()` only
4. Found hook discovery pattern (`discover_process_nats_hook_provider`) gated on `HARNX_INSTANCE_ID` — tool discovery had no equivalent
5. Reviewed existing tests — all used raw tool names or bare `*` selectors, none exercised schema→selection→routing→execution

## Root Cause

Three coupled gaps:

1. **Discovery unwired**: `NatsToolProvider::discover` existed but was never called before schema construction. The completion path used only static declarations from `Config.tools`.

2. **Config.tools cleared**: At load, `Config::load_from_file` cleared `self.tools` for NATS mode, expecting declarations to come from discovery. But discovery was never invoked.

3. **Naming drift**: Native tools registered raw names (`read`). The worker's `ServerIdentity` module did not exist, so there was no single source of truth for forward/reverse mapping between agent-visible names (`fs_read`) and wire routing (`identity_token`, raw tool).

The coverage gap: existing tests used raw names or `*` selectors with synthetic tool calls, never exercising the full path from LLM schema through selection to tool execution.

## Solution

### L1: Wire Discovery into Schema Construction

Added async NATS tool-declaration discovery with 30s TTL cache in `tool_context.rs`. Refresh happens at the single choke point `call_agent_model` (agent_loop.rs:471) before request build:

```rust
// crates/harnx-runtime/src/tool_context.rs:174-193
pub async fn refresh_nats_tool_declarations(config: &GlobalConfig, instance_id: &InstanceId) {
    // Match hook discovery: only worker process trees have NATS identity.
    if std::env::var_os(HARNX_INSTANCE_ID).is_none() {
        return;
    }

    let config_snapshot = config.read().clone();
    let active_package = config_snapshot.active_package();
    let provider = discover_nats_tool_provider_cached(
        &config_snapshot,
        instance_id,
        active_package.as_deref(),
    )
    .await;
    let declarations = provider
        .as_ref()
        .map(|provider| provider.declarations_for_use_tools(Some("*")))
        .unwrap_or_default();
    *config.read().nats_tool_declarations.write() = declarations;
}
```

Declarations merge in `Config::tool_declarations_for_use_tools`, no preload into `Config.tools`.

**Critical gate**: Discovery returns early when `HARNX_INSTANCE_ID` absent, matching hook discovery. Without this gate, non-worker paths hang on `async_nats::connect` (test hang discovered during verification).

### L2: Centralize Naming in ServerIdentity Module

Added `ServerIdentity` module as single source of truth for forward (agent-visible name) and reverse (route) tool naming:

```rust
// crates/harnx-runtime/src/server_identity.rs:7-33
pub struct ServerIdentity;

impl ServerIdentity {
    pub fn identity_token(registration: &Registration) -> String {
        server_identity_token(
            registration.package.as_deref(),
            &registration.config,
            &registration.server,
        )
    }

    pub fn agent_visible_name(
        agent_package: Option<&str>,
        registration: &Registration,
        raw_tool: &str,
    ) -> String { /* ... */ }

    pub fn parse_agent_tool_name(name: &str, known: &[Registration]) -> Option<(String, String)> { /* ... */ }
}
```

`Registration` struct gained `package: Option<String>` and `config: String` fields with `#[serde(default)]` for backward compatibility.

### L3: Authoritative Identity Injection (GitHub #1350)

Worker injects `HARNX_SERVER_PACKAGE` and `HARNX_SERVER_CONFIG` at spawn. Identity token `<pkg>__<config>__<server>` folds into KV key + subject via S1 fold (token occupies existing `<server>` slot — still 5 segments, NO proto bump):

```rust
// crates/harnx-core/src/instance.rs:tool_subject
format!("harnx.v1.{self}.tools.{identity_token}.{tool}")

// KV registration key: <instance>.<identity_token>
```

Tool request subscription uses `queue_subscribe` keyed by identity token; control stays fanout. Supervisor readiness validates echoed package/config; cleanup deletes exact identity token.

**Safe without proto bump**: Instance IDs are per-boot nonces (`InstanceId::new` = pid+uuid), so version cohorts never share a namespace.

### L4: Native/Bridge Emit Raw Names

Bridge and native toolsets emit raw tool names; `ServerIdentity` handles all composition. No `{server}_` prefixing in toolset code.

### L5: Required E2E Test

Added `model_schema_selection_routing_and_execution_use_package_tool_names` test that exercises schema→selection→routing→execution without live LLM:

- Starts real `harnx-fs-tools` server
- Verifies raw `read` registration
- Asserts same-package `fs_read` and cross-package `tools-pkg__fs_read` schema visibility
- Asserts tool execution returns file content
- **Fails on main** (no declarations), **passes on branch** (discovery wired)

```rust
// crates/harnx-runtime/tests/package_tool_naming_e2e.rs:264-267
assert!(
    discovered_names.iter().any(|tool| tool == "fs_read"),
    "live provider did not expose fs_read: {discovered_names:?}"
);
```

### Bonus Bug Found: schemars Nullable Schema Parse Failure

During e2e development, `fs_read` declaration silently dropped. Root cause: schemars emits nullable as `type: ["T", "null"]`, but `JsonSchema` parse expected scalar type.

Fix: normalize `[T, "null"]` → `T` before parse:

```rust
// crates/harnx-runtime/src/nats_tool_provider.rs:327-351
fn normalize_schema_types(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(Value::Array(types)) = object.get_mut("type") {
                let schema_type = types
                    .iter()
                    .filter_map(Value::as_str)
                    .find(|schema_type| *schema_type != "null")
                    .or_else(|| types.iter().find_map(Value::as_str));
                if let Some(schema_type) = schema_type {
                    *object.get_mut("type").expect("type key exists") =
                        Value::String(schema_type.to_string());
                }
            }
            for child in object.values_mut() {
                normalize_schema_types(child);
            }
        }
        // ...
    }
}
```

## Why This Works

1. **Discovery choke point**: `call_agent_model` is the single path where all real agent turns request completions. Wiring discovery there ensures declarations are fresh before schema construction.

2. **Env gate prevents hangs**: `HARNX_INSTANCE_ID` check mirrors hook discovery, ensuring non-worker paths (tests, one-shot commands) skip NATS connection attempts.

3. **Single naming module**: `ServerIdentity` owns all forward/reverse mapping; native toolsets and bridge do zero composition. Mapping can't drift between registration and invocation.

4. **S1 fold avoids proto bump**: Reusing an existing wire slot for richer token works because worker owns both ends and namespaces are per-instance nonces. No version negotiation needed.

5. **E2e test closes the gap**: The test exercises the exact path that shipped broken. Fail-on-main/pass-on-branch validates the fix end-to-end.

## Prevention Strategies

**Test Coverage:**
- Required e2e test for any tool-registration path change
- Schema→selection→routing→execution coverage without live LLM
- Fail-on-main validation ensures regression can't re-enter

**Code Patterns:**
- Async discovery must gate on worker identity (`HARNX_INSTANCE_ID`) to prevent hangs in non-worker contexts
- Single module for naming (`ServerIdentity`) — zero composition in callers
- Normalize schemars output before parsing; never assume scalar `type`

**Code Review Checklist:**
- [ ] Does `use_tools` schema include discovered declarations?
- [ ] Is discovery gated for non-worker paths?
- [ ] Is naming consistent between registration and routing?
- [ ] Does an e2e test cover schema→execution without live LLM?

**Test Isolation:**
- Env-mutating tests must take shared `env_lock()` and use `EnvGuard` for restoration
- Parallel test execution (`cargo nextest -j 8`) exposes races that sequential (`-j 4`) hides

## Related Issues

- **GitHub:** [#1350](https://github.com/dobesv/harnx/issues/1350) — Identity collision fixed by L3
- **Related Solution:** [integration-issues/native-nats-toolset-conversion-pattern-2026-07-31.md](./native-nats-toolset-conversion-pattern-2026-07-31.md) — S1 fs conversion pattern
- **Related Solution:** [integration-issues/stateful-toolset-conversion-2026-08-02.md](./stateful-toolset-conversion-2026-08-02.md) — Stateful server conversion
- **Related Solution:** [integration-issues/hooks-nats-launch-dispatch-complete-2026-08-01.md](./hooks-nats-launch-dispatch-complete-2026-08-01.md) — Hook discovery pattern (mirrored for tools)
