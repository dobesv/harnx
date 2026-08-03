---
title: "Hooks over NATS Core — Dual Dispatch, Fire-and-Forget Context, and Fork vs Share"
date: 2026-07-31
category: "integration-issues"
problem_type: integration_issue
component: "harnx-runtime/harnx-hookset"
root_cause: "hooks only ran inline; no NATS dispatch path existed and inline hooks could not be bypassed without breaking proxy-auth"
resolution_type: code_fix
severity: high
tags:
  - nats
  - hooks
  - dual-dispatch
  - fire-and-forget
  - testing
plan_ref: "hooks-over-nats-core"
last_updated: 2026-08-01
superseded_by: "hooks-nats-launch-dispatch-complete-2026-08-01"
---

# Solution: Hooks over NATS Core

## Problem

Hooks ran only through the inline `harnx_hooks` dispatcher. Adding a NATS dispatch path risked breaking existing inline hooks (notably `harnx-proxy-auth`, which injects GitHub/Atlassian credentials for `bash_exec`) if inline dispatch was removed or bypassed. PostToolUse context injection from async hooks had no path into the agent's turn-local state.

## Symptoms

- No NATS hook registry or dispatch mechanism existed.
- `build_dispatch_hook_fn` called only `dispatch_hooks_with_count_and_manager` (inline).
- `AgentLoopContext` allocated `pending_async_context: None` in the nats_worker path, leaving detached PostToolUse tasks no handle to inject context.
- Hook wire types (`HookPayload`, `HookOutcome`, etc.) were one-directional (Serialize-only or Deserialize-only) and could not round-trip through NATS.

## Investigation Steps

Verified the seam in `crates/harnx-runtime/src/tool.rs`:

- Engine (`harnx-engine/src/tool.rs`) calls `ctx.dispatch_hook_fn` for Pre/Post hooks — unchanged.
- `build_dispatch_hook_fn` builds the closure that calls inline dispatch.
- The closure takes `event: HookEvent` **by value** — essential for mutation composition.

Inspected toolset-server for forking:

- `harnx-toolset-server` implements idempotency-cache, cancellation, control subject, reply-cache.
- Hooks don't need these — they're single-shot dispatch, no retry semantics.

Analyzed context handle issues:

- `AgentLoopContext.pending_async_context: Option<Arc<Mutex<Option<String>>>>` is drained at turn start.
- Detached `tokio::spawn` tasks from PostToolUse cannot reach turn-local `&mut Option<String>` — must use the Arc.

Aristarchus multi-agent review confirmed:
- Engine diff empty (hard constraint holds).
- Dual dispatch preserves proxy-auth.
- Fork decision clean.
- Discovery-error → inline-only fail-open is acceptable (no NATS-only security hook exists yet).

## Root Cause

1. **Missing NATS path**: No registry, no discovery, no dispatch to remote hooks.
2. **Serde gap**: Wire types couldn't serialize + deserialize for round-trip over NATS.
3. **Context unreachable from detached task**: Turn-local async_manager is inaccessible from `tokio::spawn` — only the shared Arc works.

## Solution

### Serde Round-Trip Derives (Task 1)

Added `Deserialize` to `HookPayload`, `HookEvent`; added `Serialize`+`Deserialize` to `HookResultControl`, `HookOutcome`; added `Serialize` to `HookResult`, `HookSpecificOutput`. Kept `HookEvent`'s `#[serde(tag="hook_event_name", rename_all="PascalCase")]` unchanged.

`HookResultControl` uses serde's default externally tagged enum representation:
```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookResultControl {
    Continue,
    Block { reason: String },
    Ask { reason: Option<String> },
}
```

`FailPolicy` serializes lowercase (`#[serde(rename_all = "lowercase")]`) and defaults to `Closed`:
```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailPolicy {
    #[default]
    Closed,
    Open,
}
```

### FORK vs SHARE: harnx-hookset-server (Task 2-3)

Forked `harnx-toolset-server` into `harnx-hookset-server` rather than extracting shared code:

- **Why fork**: Tool-serving carries idempotency-cache, cancellation, control-subject, reply-cache — none of which hooks need.
- **HookRegistration mirrors Registration**: Server name + hooks list + schema/proto versions.
- **Subject**: `harnx.v1.{instance}.hook.{server}.{event}` (parallel to tool subject pattern).
- **Registry**: Separate `harnx_hook_registry` KV bucket.
- **Refresh**: 30-second TTL (same pattern as tool registry).

Shared KV/registration utilities could be extracted later at the config-migration slice, but forking keeps the walking skeleton simple.

### Dual Dispatch Pattern (Task 4-5)

Added NATS dispatch to `build_dispatch_hook_fn` without breaking inline hooks:

**PreToolUse (sequential, mutation-chained):**
1. Run NATS hooks first.
2. If NATS returns `Block` or `Ask`, short-circuit — return that outcome. Inline hooks **do not run** (inline proxy-auth can't override NATS denial).
3. If NATS returns `Continue` with `mutated_tool_input`, rebuild the `HookEvent::PreToolUse` with the mutated input.
4. Feed the NATS-mutated event into the existing inline `dispatch_inline_hooks`.
5. Compose outcomes: if inline's mutation is `None`, use NATS mutation; otherwise inline's mutation wins (inline saw NATS-mutated input).

**Why closure by value matters:** The dispatch closure takes `event: HookEvent` by value, which lets us construct a fresh `HookEvent` from the NATS outcome and pass it to inline dispatch. This is the key enabler for NATS-then-inline composition.

**PostToolUse (fire-and-forget):**
1. NATS dispatch spawns `tokio::spawn` per matching hook — returns immediately.
2. Inline hooks run against the original event (no mutation from NATS to apply).
3. NATS outcomes:
   - Errors emit `AgentEvent::Notice(NoticeEvent::Error)`.
   - `mutated_tool_response` is **dropped** (acceptable behavior change).
   - `additional_context`/`system_message` append to `pending_async_context`.

**Shared context handle routing:**
```rust
// crates/harnx-runtime/src/nats_worker/agent_loop.rs
pending_async_context: Some(Arc::new(tokio::sync::Mutex::new(None))),
```

The Arc flows through:
- `ToolRoundParams.pending_async_context`
- `BuildToolEvalContextParams.pending_async_context`
- `build_dispatch_hook_fn` captures it
- `dispatch_post_tool_use` passes it to `dispatch_one_post_hook`
- Detached task calls `pending.lock().await` to append

**No NATS client fallback:**
If `nats_hook_provider` is `None`, `build_dispatch_hook_fn` falls back to inline-only. Non-NATS callers (tests) continue working without NATS setup.

### Registry/Discovery Mirror (Task 4)

`NatsHookProvider::discover` mirrors `NatsToolProvider::registration_snapshot`:
- Query `harnx_hook_registry` for `{instance}.*` keys.
- Filter: `event` matches event name, `matcher` (regex) matches **bare tool name**.
- Sort: `priority` ascending, then server name (deterministic tiebreak).

```rust
// crates/harnx-runtime/src/nats_hook_provider.rs
fn matching_hooks<'a>(
    hooks: &'a [DiscoveredHook],
    event_name: &str,
    tool_name: Option<&str>,
) -> Vec<DiscoveredHook> {
    // ... filter by event + matcher ...
    matches.sort_by(|(left_index, left), (right_index, right)| {
        left.spec.priority.cmp(&right.spec.priority)
            .then_with(|| left.server.cmp(&right.server))
            .then_with(|| left_index.cmp(right_index))
    });
    matches.into_iter().map(|(_, hook)| hook).collect()
}
```

### Injectable Dispatch Seam (Task 4)

The `HookRequestDispatcher` trait enables unit tests without a live NATS server:

```rust
#[async_trait]
trait HookRequestDispatcher: Send + Sync {
    async fn request(&self, subject: String, payload: Vec<u8>, timeout: Duration) -> Result<HookOutcome>;
}
```

Production: `NatsHookRequester` wraps `async_nats::Client`.
Tests: `StubDispatcher` returns queued outcomes and records seen inputs.

## Why This Works

**Dual dispatch preserves proxy-auth:** NATS hooks run first, but inline hooks still run after NATS passes. Proxy-auth (an inline hook) continues to receive events and can mutate or block. NATS can't silently bypass the security layer.

**Fire-and-forget doesn't block tool completion:** `dispatch_post_tool_use` spawns detached tasks and returns immediately. The tool round completes without waiting for PostToolUse hooks.

**Shared Arc enables context injection:** The `Arc<Mutex<Option<String>>>` is reachable from any spawned task. The take-then-put pattern preserves existing context:

```rust
let mut guard = pending.lock().await;
let mut accumulated = guard.take().unwrap_or_default();
// ... append new context ...
*guard = Some(accumulated);
```

**Discovery with no provider:** When no `HARNX_INSTANCE_ID` is set (frontends without NATS), `dispatch_hook_event` returns `Continue` without hook enforcement by design. A normal empty registry (no hooks configured) means no hooks run. If discovery/registry read fails while a provider is set, the fail-closed guard blocks `UserPromptSubmit` and `PreToolUse` — see `hooks-nats-launch-dispatch-complete-2026-08-01.md` for the expectations-manifest pattern.

## Accepted Behavior Changes / Known Gaps

1. **PostToolUse mutated_tool_response dropped**: NATS PostToolUse hooks cannot mutate the tool response. Logged and ignored. Restoring this is deferred.

2. **~~PreToolUse additional_context/system_message not yet injected~~ — RESOLVED**: NATS PreToolUse hooks now route these fields via `ContinueResultAccumulator`. See `hooks-nats-launch-dispatch-complete-2026-08-01.md`.

3. **Ask-over-NATS headless edge**: `Ask` routes through `ToolApprovalRequiredError`, but headless-worker resolution needs design. Deferred.

4. **~~Discovery-error → inline-only~~ — RESOLVED (Phase 4)**: Inline path removed. `dispatch_hook_event` with no provider returns Continue and logs warning. Frontends without `HARNX_INSTANCE_ID` get no hook enforcement by design.

5. **Registry trust model**: Mirrors tool registry — any process with the NATS token can register. Trust is per-broker, not per-hook.

6. **Matcher uses bare tool name**: Hook specs match `bash_exec` or `fs_read`, not prefixed display names. Renaming an MCP server doesn't require updating matchers.

7. **~~FailPolicy::Closed fail-open on crash~~ — RESOLVED (Phase 4)**: Expectations manifest (`harnx_hook_expectations` KV bucket) ensures missing required Closed hooks fail closed. See `hooks-nats-launch-dispatch-complete-2026-08-01.md`.

## Prevention Strategies

**Test coverage:**
- `nats_hooks_e2e.rs`: end-to-end with spawned NATS, test hook, and worker dispatch.
- Unit tests for mutation-chaining, Block short-circuit, Post fire-and-forget context append.
- `HookRequestDispatcher` trait enables tests without NATS binary.

**Code review checklist:**
- [ ] Are shared contexts (`Arc<Mutex<...>>`) allocated before dispatch?
- [ ] Are matcher comparisons against bare tool names?
- [ ] Does expectations manifest cover all required Closed hooks?

**Monitoring:**
- Hook registry bucket exists and contains expected keys.
- Hook timeouts and fail_policy surfaces correctly.
- PostToolUse context reaches pending_async_context.

## Related Issues

- **GitHub:** [#1224](https://github.com/dobesv/harnx/issues/1224) — Native migration umbrella.
- **Prior solution:** `nats-instance-scoped-tool-servers-2026-07-29.md` — Instance-scoped tool servers and registration pattern.
- **Deferred:** InstructionsLoaded/CwdChanged firing, PostToolUse response mutation.
