---
title: "Hook config command-only pattern — shared-struct field removal, fail-closed fallback chains, and nonce-as-name lifecycle"
date: 2026-08-02
category: "api-design"
problem_type: integration_issue
component: "harnx-runtime, harnx-hooks, harnx-sandbox-run"
root_cause: "shared-struct field removal missed secondary consumer; fail-closed single-point-of-failure in marker publish; opaque hook names impeded readiness correlation and dispatch ordering"
resolution_type: code_fix
severity: high
tags:
  - hooks
  - nats
  - fail-closed
  - lifecycle
  - supervisor
  - shared-struct
  - api-design
plan_ref: "hooks-command-only-config"
---

## Problem

Refactoring hook config to "command-only" (removing `type`, `event`, `matcher`, `timeout`; the running server self-declares over NATS) broke a secondary inline dispatch consumer that recon missed, exposed subtle fail-closed invariant gaps when crash-marker publication itself failed, and required a unified naming scheme for readiness correlation and dispatch ordering.

## Symptoms

**Phase 3a recon gap:**
- Build failed after removing `event`/`matcher`/`timeout` from `HookConfig`
- `crates/harnx-hooks/src/dispatch.rs` used `.event` and compiled `.matcher` for the inline sandbox-run path
- `crates/harnx-hooks/src/executor.rs` used `.timeout` for the inline executor
- No test caught it because sandbox-run tests built from their own CLI `--hook` convention, not from `HookConfig`

**Fail-closed review cycles:**
- Cycle 1: crash-marker publish failure could leave a self-declared-Open crashed hook fail-open (marker publish failed → live registration retained → Open policy served)
- Cycle 1: deleting the registration without publishing a rejector left an empty discovery snapshot (fail-open)
- Cycle 2: the two-tier fallback (`HOOK_EXPECTATIONS_BUCKET` → `HOOK_REGISTRY_BUCKET`) had no integration test coverage

**Name correlation gap:**
- Old design passed no per-launch identifier; readiness watch couldn't distinguish "my child" from stale registry replay
- Equal-priority dispatch order relied on registry insertion order, which wasn't deterministic across worker restarts

## Investigation Steps

### 1. Shared-struct field removal

Searched for all readers of the fields being removed:
```sh
rg '\.event' crates/harnx-hooks/src/dispatch.rs
rg '\.matcher' crates/harnx-hooks/src/dispatch.rs
rg '\.timeout' crates/harnx-hooks/src/executor.rs
```

Found the inline sandbox-run dispatch path (`dispatch_hooks`, `dispatch_hooks_with_count_and_manager`) filtered by `hook.event != event.event_name()` and compiled `hook.matcher` into a regex. The NATS path doesn't use inline dispatch — it dispatches over NATS — but `HookConfig` was the shared source for both.

Evaluated options:
- Re-add serde-skipped fields to `HookConfig` → leaks inline concern into NATS domain model
- Create explicit inline metadata carrier → cleanest, isolates concerns

### 2. Fail-closed invariants

Traced crash path in `spawn_child_monitor`:
```rust
// Original: marker publish failure → retain live registration (fail-open for Open hooks)
let marker_published = publish_marker(&marker).await.is_ok();
if !marker_published {
    // PROBLEM: live registration stays, policy=Open means fail-open
}
remove_registration(&key).await;
```

Confirmed design requirement:
- A crashed hook must block dispatch regardless of its self-declared policy
- Empty discovery snapshot must also fail closed (no routes → reject)

Two-tier fallback pattern emerged:
1. Try `HOOK_EXPECTATIONS_BUCKET` (normal crash marker path)
2. On failure, publish synthetic rejector to `HOOK_REGISTRY_BUCKET`
3. Only then delete the live registration

### 3. Name as correlation token

Existing server comparison:
```rust
// In provider: tiebreak by server name when priorities equal
servers.sort_by(|a, b| a.server.cmp(&b.server));
```

Supervisor could assign deterministic names if zero-padded:
```rust
fn hook_server_name(run_id: &str, order_index: usize) -> String {
    format!("hook-{run_id}-{order_index:03}")
}
```

This yields `hook-a1b2c3d4-000`, `hook-a1b2c3d4-001`, etc. — lexical sort equals numeric sort, reproducing config declaration order.

## Root Cause

**Struct field removal gap:** The inline dispatcher and NATS supervisor both read from `HookConfig`. Removing fields without auditing all consumers breaks the path that doesn't self-register over NATS.

**Fail-closed gap:** A single-point-of-failure in the crash marker publish can violate the fail-closed invariant. The fallback must guarantee fail-closed even when the bucket is unavailable.

**Name correlation gap:** Without supervisor-assigned names matching the readiness watch key, the supervisor cannot distinguish a stale registration (previous incarnation) from its own child. A per-launch nonce closes this gap.

## Solution

### 1. InlineHookSpec for the inline consumer

Created an explicit struct instead of serde-skipped fields on `HookConfig`:

```rust
// crates/harnx-hooks/src/dispatch.rs
pub struct InlineHookSpec {
    pub event: String,
    pub matcher: Option<String>,
    pub command: HookCommand,
    pub async_hook: Option<bool>,
}
```

Changed `dispatch_hooks` and variants to take `&[InlineHookSpec]`. Sandbox-run builds `InlineHookSpec` from its `HookDef` CLI convention, not from `HookConfig`. The NATS path never uses this — it dispatches over NATS.

Key insight: don't leak a consumer-specific concern into the shared domain model.

### 2. Fail-closed fallback chain

Crash handler publishes marker or rejector, never leaves an Open policy alive:

```rust
// crates/harnx-runtime/src/nats_worker/hook_supervisor.rs
let route = publish_marker_or_rejector(
    || async {
        let expectations = ensure_bucket(&client, HOOK_EXPECTATIONS_BUCKET).await?;
        publish_registration(&expectations, &key, &marker).await
    },
    || publish_crash_rejector(&client, &instance_id, &rejector_name, &rejector_label),
).await;

match route {
    Ok(CrashRoute::Marker) => remove_registration(&client, &instance_id, &server).await,
    Ok(CrashRoute::Rejector) => {
        log::warn!("crash marker failed; installed rejector");
        remove_registration(&client, &instance_id, &server).await;
    }
    Err(error) => {
        // Last resort: live registration retained but logging. Provider cache expires in ≤30s.
        log::error!("failed to install any fail-closed route: {error:#}");
    }
}
```

`publish_crash_rejector` tries `HOOK_EXPECTATIONS_BUCKET`, then falls back to `HOOK_REGISTRY_BUCKET`:

```rust
pub async fn publish_crash_rejector(
    client: &async_nats::Client,
    instance_id: &InstanceId,
    server: &str,
    display_label: &str,
) -> Result<()> {
    let registration = fail_closed_rejector(server, display_label);
    let key = hook_registration_key(instance_id, server);

    let expectation_result = async {
        let expectations = ensure_bucket(client, HOOK_EXPECTATIONS_BUCKET).await?;
        publish_registration(&expectations, &key, &registration).await
    }.await;
    if expectation_result.is_ok() {
        return Ok(());
    }

    // Fallback: registry bucket as second publication path
    let registry = ensure_bucket(client, HOOK_REGISTRY_BUCKET).await?;
    publish_registration(&registry, &key, &registration).await
}
```

The rejector has two closed specs (priority 0, no timeout):
- `UserPromptSubmit` (no matcher)
- `PreToolUse` (matcher `.*`)

### 3. Nonce-as-name dual-purpose pattern

Supervisor generates one `run_id` per `start_local_with_timeout`, derives all hook names from it:

```rust
let run_id = Uuid::new_v4().simple().to_string()[..8].to_string();
let launch_plan: Vec<_> = enabled
    .into_iter()
    .enumerate()
    .map(|(order_index, hook)| HookLaunch {
        order_index,
        name: hook_server_name(&run_id, order_index),  // hook-{run_id}-{NNN}
        hook,
    })
    .collect();
```

Name functions:
1. **Readiness correlation:** The per-name registry watch verifies `registration.server == assigned_name` before declaring success
2. **Dispatch-order tiebreak:** Zero-padded `NNN` means lexical sort (`000 < 001 < ...`) equals declaration order

Passed to every server via `--name`, including `harnx-proxy-auth` (which no longer hardcodes its name).

### 4. Startup-failure rejector

When a hook never registers (crash before healthy, spawn failure, readiness timeout):

```rust
// crates/harnx-runtime/src/nats_worker/hook_supervisor.rs — rejector construction
let rejector = fail_closed_rejector(&rejector_name, &failure_label);
let key = hook_registration_key(&instance_id, &rejector_name);
let expectations = ensure_bucket(&client, HOOK_EXPECTATIONS_BUCKET).await?;
publish_registration(&expectations, &key, &rejector).await?;
```

Display label: `"hook server failed to start: {status_message ?? command}"` (bounded to 120 chars).

## Why This Works

**InlineHookSpec:** The inline sandbox-run path has its own metadata carrier, isolating it from the NATS supervisor's `HookConfig`. Neither path accidentally depends on fields the other doesn't use.

**Fail-closed chain:** Even if `HOOK_EXPECTATIONS_BUCKET` is unavailable, the rejector lands in `HOOK_REGISTRY_BUCKET`. Discovery always sees at least one closed route. The only gap is an existing provider's cached entry (≤30s refresh), which is the accepted design caveat.

**Nonce-as-name:** One token serves two purposes without divergence. Expectation prep and spawn share the same `name` derivation, so readiness watch and registration are always aligned.

## Prevention Strategies

**Field removal checklist:**
- [ ] Grep ALL field accesses (`rg '\.field_name'`) across the whole crate tree
- [ ] Check test fixtures and example configs
- [ ] Verify secondary paths (inline dispatch, CLI tools) don't read the shared struct
- [ ] Build the full workspace (`cargo build --workspace`) before running tests

**Fail-closed review:**
- [ ] Every crash path must publish a blocking route before deleting the live registration
- [ ] Publish fallback chain must have at least two buckets when one may be unavailable
- [ ] Integration test must cover bucket-unavailable fallback branches

**Integration test hygiene (repo-specific):**
- [ ] Run `cargo build --workspace` before `cargo nextest run --workspace` — piecemeal `-p` builds omit e2e prerequisites like `harnx-mcp-time`, `harnx-mock-mcp`
- [ ] Avoid `pkill -9` during a build — it can delete freshly-linked test binaries

**Non-deterministic name check:**
- [ ] Server names used for ordering must be zero-padded to match declaration order
- [ ] Per-launch unique prefix prevents stale replay false positives

## Related Issues

- GitHub: [#1224](https://github.com/dobesv/harnx/issues/1224) — umbrella hooks config migration issue
- `integration-issues/hooks-nats-launch-dispatch-complete-2026-08-01.md` — Phase 4: launch, lifecycle, and context aggregation
- `workflow-issues/removing-dead-config-fields-rust-2026-05-05.md` — general field removal checklist
