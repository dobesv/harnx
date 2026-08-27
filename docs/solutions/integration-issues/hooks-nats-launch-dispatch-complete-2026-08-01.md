---
title: "Hooks over NATS Launch and Dispatch — Supervisor Lifecycle, Unified Entrypoint, and Context Aggregation"
date: 2026-08-01
category: "integration-issues"
problem_type: integration_issue
component: "harnx-runtime, harnx-hookset, harnx-proxy-auth"
root_cause: "hooks only partially dispatched over NATS; no worker-side launch/lifecycle management for hook servers; PreToolUse context aggregation silently dropped across multi-hook sequential dispatch"
resolution_type: code_fix
severity: high
tags:
  - nats
  - hooks
  - lifecycle
  - supervisor
  - context-aggregation
  - handoff
plan_ref: "hooks-config-migration"
---

# Solution: Hooks over NATS Launch and Dispatch

## Problem

Hooks over NATS were incomplete: only PreToolUse/PostToolUse dispatched over NATS, other events fired inline directly. No worker-side launch mechanism existed for hook servers — global, tool-server, and agent hooks had no lifecycle management. PreToolUse `additional_context`/`system_message` were silently dropped when multiple hooks chained sequentially because the dispatch loop only accumulated `mutated_tool_input`.

## Symptoms

- `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `Stop`, `StopFailure` events never reached NATS hooks
- The historical in-place agent handoff kept old agent hooks registered while
  the target agent's hooks never started
- PreToolUse hooks returning `additional_context` or `system_message` had those fields silently discarded
- No tests caught the context-drop bug because tests bypassed the public `dispatch_event` entrypoint
- `FailPolicy::Closed` hooks that crashed or failed to start were removed from registry, dispatch treated "no matching hooks" as Continue (fail-open for security hooks)

## Investigation Steps

1. Traced `dispatch_hook_event` call sites: `tool.rs`, `agent_loop.rs`, `commands.rs`, `main.rs` — only some called NATS dispatch, others fired inline directly.

2. Found `HookServerSupervisor` mirrored `ToolServerSupervisor` correctly, but no code started it at the right lifecycle seams:
   - Global hooks needed startup in `run_worker_daemon`
   - Tool-server hooks needed co-launch with their parent tool server
   - Agent hooks needed session-bind reconciliation; the historical in-place
     handoff also needed an explicit swap

3. Traced `dispatch_pre_tool_use_with` (the multi-hook sequential loop): Continue arm accumulated `mutated_tool_input` but ignored `additional_context`/`system_message`/`resume`. The helper `continue_outcome(mutated_tool_input)` returned a `HookOutcome` with all context fields defaulted to `None`.

4. Verified the historical `reconcile_agent_hooks` handoff swap had zero test
   coverage — no test ever produced a `HandoffRequested` result.

5. Confirmed that `pending_async_context` is a standalone `Arc<Mutex<Option<String>>>` owned by agent loop, NOT tied to `AsyncHookManager`. This independence is why NATS context injection works without the inline async manager.

## Root Cause

**PreToolUse context drop:** `dispatch_pre_tool_use_with` returned `continue_outcome(final_mutation)` which hardcodes `..HookResult::default()` for all fields except `mutated_tool_input`. Context fields were discarded before `append_outcome_context` ever saw them.

**Launch seams wrong:** Developers assumed `use_agent`/`exit_agent` (frontend operations) were the hook launch seams. In a NATS worker, those run in the frontend. The actual seams are:
- `run_worker_daemon` for global hooks (instance lifetime)
- `ToolServerSupervisor::start_local` for tool-server hooks (co-launched, co-dropped)
- `load_or_repair_session` / `attach_session_to_config` for agent hooks (session bind)
- ordinary target-session activation for handoffs (reconciles independently)

Worker readiness is intentionally announced before the daemon's background
services finish registering. That cannot extend to the first activation:
`SessionStart` discovery is one-shot, so a cold worker that activates before
global hook registration can permanently miss the event. Claimed activations
therefore wait at `await_initial_background_services` for the bounded initial
registration attempt before acknowledging and running the session. Later
activations do not pay this startup barrier.

**Historical handoff gap:** The original recursive handoff path lacked a reconcile step, so a handed-off session retained old agent registrations. Handoffs now activate an independent target session through the ordinary worker lifecycle, while the source hook lifecycle remains untouched.

## Solution

### 1. Unified `dispatch_hook_event` Entrypoint

All hook events now route through a single entrypoint:

```rust
pub async fn dispatch_hook_event<F>(
    event: HookEvent,
    provider: Option<&NatsHookProvider>,
    meta: HookDispatchMeta,
    pending_async_context: Option<Arc<Mutex<Option<String>>>>,
    inline_fallback: F,
) -> HookOutcome
where
    F: Future<Output = HookOutcome>,
```

When `provider` exists, NATS dispatch runs. When `None`, inline fallback runs. This covers:
- Tool dispatch (`tool.rs`)
- Agent-loop events (`agent_loop.rs`: UserPromptSubmit, Stop, StopFailure)
- CLI commands (`commands.rs`: `/hook` command)
- Session lifecycle (`main.rs`: SessionStart, SessionEnd)

### 2. HookServerSupervisor Lifecycle Scopes

`HookServerSupervisor` mirrors `ToolServerSupervisor`:

```rust
pub struct HookServerSupervisor {
    handles: Vec<HookServerHandle>,
    instance_id: InstanceId,
    jetstream_ctx: jetstream::Context,
    client: async_nats::Client,
}

pub struct HookServerStartConfig {
    nats_url: String,
    token: String,
}
```

**Launch routing:**
- Global hooks: `run_worker_daemon` starts from `config.hooks`, retained in `WorkerRuntime._global_hook_supervisor`
- Tool-server hooks: `ToolServerSupervisor` owns `hook_supervisors` map, co-launched with tool server, same package dir, drop together
- Agent hooks: Session bind (`attach_session_to_config`) starts from `agent_resolved_hooks` (strips global prefix to avoid duplicates), stored in `AgentLoopSegmentArgs.hook_supervisor`

**Native proxy-auth routing:** Command basename check — if `harnx-proxy-auth`, launch that binary directly with remaining CLI flags. Fixed server name `proxy-auth`. All other commands route to `harnx-claude-compatible-hook-server`.

### 3. ContinueResultAccumulator Pattern

Fixed PreToolUse context drop with explicit aggregation:

```rust
struct ContinueResultAccumulator {
    additional_contexts: Vec<String>,
    system_messages: Vec<String>,
    resume: Option<bool>,
}

impl ContinueResultAccumulator {
    fn push(&mut self, result: &HookResult) {
        if let Some(ctx) = &result.additional_context {
            if !ctx.is_empty() {
                self.additional_contexts.push(ctx.clone());
            }
        }
        if let Some(msg) = &result.system_message {
            if !msg.is_empty() {
                self.system_messages.push(msg.clone());
            }
        }
        self.resume = Some(self.resume.unwrap_or(false) || result.resume.unwrap_or(false));
    }

    fn into_result(self, mutated_tool_input: Option<serde_json::Value>) -> HookResult {
        HookResult {
            mutated_tool_input,
            additional_context: if self.additional_contexts.is_empty() {
                None
            } else {
                Some(self.additional_contexts.join("\n"))
            },
            system_message: if self.system_messages.is_empty() {
                None
            } else {
                Some(self.system_messages.join("\n"))
            },
            resume: self.resume,
            ..HookResult::default()
        }
    }
}
```

`dispatch_pre_tool_use_with` now calls `accumulated.push(&outcome.result)` for each Continue outcome and returns `accumulated.into_result(final_mutation)`.

Block/Ask short-circuit unchanged (`return outcome` immediately).

### 4. Handoff Hook Reconciliation

The original implementation explicitly called a hook-reconciliation helper
from `prepare_nats_handoff`. The current detached architecture removed that
swap: the source supervisor remains scoped to the source activation, and the
target's ordinary activation calls the private `reconcile_agent_hooks` adapter
after loading the target agent. That adapter resolves the agent's current hooks
and session scope before calling this lifecycle primitive:

```rust
#[doc(hidden)]
pub async fn reconcile_hook_supervisor(
    current: &mut Option<HookServerSupervisor>,
    start: Option<&HookServerStartConfig>,
    hooks: &HooksConfig,
    scope: &str,
)
```

The primitive mutates the owned supervisor in place. Sequential stop-then-start
prevents old/new overlap. A brief fail-open window exists between stop and start
and is acceptable for the swap semantics.

### 5. pending_async_context Architecture

`pending_async_context` is a standalone `Arc<Mutex<Option<String>>>` owned by agent loop:

- NATS PreToolUse appends via `append_outcome_context`
- NATS PostToolUse appends via `append_pending_context` (fire-and-forget, spawned)
- Inline async manager removed in Phase 4 won't break context injection — the Arc is independent

This separation allows inline path removal without breaking async context flow.

### 6. Ask-over-NATS Behavior

Ask returns as `HookResultControl::Ask`. Engine's `confirm_tool_use_fn` converts to `ToolApprovalRequiredError`. Headless workers cannot prompt — the existing confirmation callback decides whether to surface or deny. Ask does not silently proceed.

### 7. proxy-auth NATS Mode

`harnx-proxy-auth` selects NATS mode only when all three of `HARNX_INSTANCE_ID`, `HARNX_NATS_URL`, and `HARNX_NATS_TOKEN` are present. A partial NATS environment falls back to the stdin-JSONL loop instead of returning a configuration error:

- `ProxyAuthHook` implements `Hook` trait
- Registration: `PreToolUse` with matcher `exec|spawn`, priority 0, `FailPolicy::Closed`
- `handle_hook` calls existing `augment_tool_input`, returns mutation
- Process must stay alive for proxy port + TempDir-backed CA/creds lifetime
- Local-only: injected proxy URL is `127.0.0.1:<ephemeral-port>`, requires co-location

## Why This Works

**Unified entrypoint simplifies all call sites:** One function (`dispatch_hook_event`) handles NATS/inline routing. Phase 4 inline removal is straightforward: delete the `inline_fallback` parameter and branch.

**Lifecycle scopes match ownership:** Global = instance, tool-server = tool server, agent = session. Hook processes die with their owner.

**Context aggregation preserves multi-hook guidance:** `ContinueResultAccumulator` joins non-empty strings with newline across all matching hooks in priority order. Each hook's contribution survives the chain.

**pending_async_context independence enables Phase 4:** Inline async manager removal won't break context flow because NATS writes directly to the Arc.

**Detached handoff preserves hook ownership:** The source supervisor remains
owned by the source activation. The independently activated target loads its
own hook configuration and reconciles that session's supervisor before it runs
the queued prompt.

## Accepted Behavior Changes / Known Gaps

1. **~~FailPolicy::Closed fail-open on crash/startup failure~~ — RESOLVED (Phase 4):** Now uses `harnx_hook_expectations` KV manifest. Supervisor publishes required fail-closed hook specs before spawning; `NatsHookProvider::discover` merges any expected-but-unregistered hook into the discovery result. A missing or crashed hook configured with `FailPolicy::Closed` that the supervisor was asked to launch makes its matched PreToolUse dispatch fail closed (Block) rather than silently continuing. Genuine KV read failure (distinguished via JetStream `STREAM_NOT_FOUND` 10059) also installs a fail-closed guard instead of failing open.

2. **Target hook startup:** A handoff uses the normal target-session bind path,
   including its existing hook reconciliation and failure-policy behavior. It
   does not introduce a cross-session stop-then-start window.

3. **Ask headless limitation:** Headless workers surface Ask via `ToolApprovalRequiredError` but cannot interactively prompt. Confirmation callback decides denial/surface.

4. **InstructionsLoaded/CwdChanged defined but not fired:** Variants exist in enum and dispatch match, but no call site constructs them. Deferred wiring.

5. **proxy-auth localhost-only:** Ephemeral port on `127.0.0.1` requires hook and tool servers on same host. Local deployment by design.

## Prevention Strategies

**Test coverage:**
- `dispatch_event_queues_aggregated_pre_tool_context_for_next_turn`: Drives public `dispatch_event` entrypoint with 2 hooks, asserts joined context/system fields and pending_async_context
- `dispatch_event_runs_best_effort_session_start`: Drives `dispatch_event` for SessionStart, asserts dispatcher called
- `handoff_to_agent_with_hooks_starts_its_hook_enforcement`: Live NATS test that
  detached target activation discovers and enforces the target agent's hooks
- `new_remote_session_fires_session_start_hook_exactly_once`: Exercises the
  cold-worker startup boundary and proves the one-shot event is neither missed
  nor duplicated

**Code patterns:**
- Always aggregate across Continue outcomes — never hardcode `..HookResult::default()` on returned outcome
- Test the public entrypoint, not just internal helpers
- Launch seams are worker-side (daemon, tool supervisor, and ordinary target
  session bind), not frontend (`use_agent`)

**Code review checklist:**
- [ ] Does `dispatch_pre_tool_use_with` accumulate `additional_context`/`system_message`?
- [ ] Does a handoff leave source hooks untouched and activate the target
      through the ordinary session-bind reconciliation path?
- [ ] Are agent hooks started at session bind, not frontend agent switch?
- [ ] Is `pending_async_context` allocated before NATS dispatch called?
- [ ] Do multi-hook tests assert on aggregated context, not just mutation?

**Monitoring:**
- Hook registry bucket size/entries
- Hook server crashes (log warning)
- PreToolUse context reaching pending_async_context
- Agent handoff hook reconciliation completion

## Related Issues

- **GitHub:** [#1224](https://github.com/dobesv/harnx/issues/1224) — Umbrella issue
- **Prior solution:** `hooks-over-nats-core-2026-07-31.md` — Initial NATS dispatch path
- **Completed (Phase 4):** Inline runtime dispatch removed; bash proxy-auth migrated to launched NATS hook via `hooks:` on tool-server config; expectations manifest closes fail-open gap
