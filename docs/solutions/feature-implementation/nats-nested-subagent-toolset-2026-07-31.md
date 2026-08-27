---
title: "NATS nested sub-agent toolset: ThinClientSession delegation and parent-stream event publishing"
date: 2026-07-31
category: "feature-implementation"
problem_type: integration_issue
component: "nats-worker, subagent_toolset"
root_cause: "ACP stdio sub-agents replaced with NATS agent sessions; ThinClientSession for nesting, explicit parent-event publishing for detached toolset handlers"
resolution_type: code_fix
severity: high
tags:
  - sub-agent
  - nats-worker
  - ThinClientSession
  - event-sink
  - handoff-vs-nesting
  - parent-session-publishing
  - idle-timeout
plan_ref: "nats-subagent-migration"
---

## Problem

Sub-agents historically ran as ACP stdio child processes via `AcpManager`. The migration to NATS required a different delegation model: sub-agents are now normal NATS agent sessions, but the parent must remain alive and leased while the child executes. Session handoff is unsuitable because it finishes the source turn and detaches the target instead of waiting for a child response.

## Symptoms

- Previous ACP path used `AcpManager::call_tool` which captured and re-entered `current_agent_event_sink()` for event forwarding
- Handoff (`_session_handoff`) finishes the parent turn and activates a detached target — it cannot return a nested result to the parent
- `ThinClientSession::run_turn` had no built-in idle or operation timeout handling
- Toolset-server handlers run detached from the parent turn task's task-local `SCOPED_SINK`

## Investigation Steps

1. **Handoff vs. nesting decision**: The implementation at the time recursively mutated the source worker through `prepare_nats_handoff`; the current handoff architecture instead queues an independent target and finishes the source. Both models are wrong for nested sub-agents, which must keep the parent turn open and return the child's result.

2. **ThinClientSession as the nesting primitive**: Confirmed `ThinClientSession::new` + `run_turn` provides a "initiate + await a nested NATS turn" abstraction without modifying the parent. The child turn routes through the WorkQueue (`SessionActivate` → `WORK_NOTIFY_<cluster>`) and executes on any eligible worker — no same-worker dependency.

3. **Self-loop deadlock check**: Verified tool calls run via `join_all(...)` in `crates/harnx-engine/src/tool.rs` (not detached `tokio::spawn`), and execution dispatches through the WorkQueue. The in-process send and serve run on independent tokio tasks — deadlock-free.

4. **Event sink context mismatch**: Reviewed tool execution in `harnx-engine/src/tool.rs:255-260` — in-engine tool calls inherit the parent's task-local `SCOPED_SINK`. Worker toolset-server handlers run on a detached server task, so `current_agent_event_sink()` returns the handler's sink (or none), not the parent's.

5. **Parent-session-id injection path**: Traced `NatsToolProvider` → `harnx_toolset_server::serve_with_client`. The toolset-server plumbing auto-injects `__harnx_parent_session_id` from `config.session` into the `InvocationRequest`. The handler deserializes it via `#[serde(rename = "__harnx_parent_session_id")]` in `NewSessionArgs` / `PromptArgs`.

6. **Timeout implementation**: `ThinClientSession` lacks native timeouts. Built idle (300s) + operation (3600s) timeout wrapping around `run_turn` with env overrides (`HARNX_SUBAGENT_IDLE_TIMEOUT_SECS`, `HARNX_SUBAGENT_OPERATION_TIMEOUT_SECS`). `ActivitySink` resets idle deadline on every child `AgentEvent`.

## Root Cause

Sub-agent delegation requires nested execution that preserves an active parent
turn. Handoff finishes that turn and detaches the target — the wrong
abstraction. `ThinClientSession` provides the correct nesting primitive. The
event-publishing path differs between in-engine tool calls (task-local sink
inheritance) and worker toolset-server handlers (detached execution requiring
explicit parent-session-id injection + direct publish to
`sessions.{parent_id}.events`).

## Solution

### ThinClientSession for Nesting

`SubagentToolset::run_prompt` creates a `ThinClientSession`, optionally emits an early `SubAgentStarted` event, then awaits the child turn with timeout wrappers:

```rust
// crates/harnx-runtime/src/nats_worker/subagent_toolset.rs:131-195
async fn run_prompt(
    &self,
    message: &str,
    session_id: Option<String>,
    parent_session_id: Option<String>,
    cancel: CancellationToken,
) -> Result<ThinClientTurnResult, ToolInvokeError> {
    let session = self.create_session(session_id).await?;
    let child_session_id = session.session_id().to_string();
    if let Some(parent_session_id) = parent_session_id {
        self.emit_parent_subagent_started(&parent_session_id, &child_session_id).await?;
    }
    // ActivitySink for idle reset, timeout loops, cancel propagation...
}
```

### Parent-Stream Event Publishing

The handler explicitly publishes to the parent session's event subject:

```rust
// crates/harnx-runtime/src/nats_worker/subagent_toolset.rs:197-243
async fn emit_parent_subagent_started(
    &self,
    parent_session_id: &str,
    child_session_id: &str,
) -> Result<(), ToolInvokeError> {
    let source = AgentSource {
        agent: self.agent.clone(),
        session_id: Some(child_session_id.to_string()),
        model: None,
    };
    let event = AgentEvent::sub_agent(
        source,
        AgentEvent::Turn(TurnEvent::SubAgentStarted {
            agent: self.agent.clone(),
            session_id: child_session_id.to_string(),
        }),
    );
    let envelope = AdvisoryEnvelope::new(after_seq, event);
    self.client
        .publish(events_subject(parent_session_id), envelope.to_bytes()?.into())
        .await?;
    self.client.flush().await?;
}
```

### Parent-Session-ID Auto-Injection

Toolset-server injects the parent session ID so handlers can publish back:

```rust
// crates/harnx-toolset-server/src/lib.rs:204-213
if request.tool.ends_with("_session_prompt") || request.tool.ends_with("_session_new") {
    if let (Some(parent_session_id), Some(args)) =
        (request.parent_session_id, args.as_object_mut())
    {
        args.insert("__harnx_parent_session_id".to_string(), Value::String(parent_session_id));
    }
}
```

Handlers deserialize with renamed field:

```rust
// crates/harnx-runtime/src/nats_worker/subagent_toolset.rs:380-393
#[derive(Deserialize)]
struct NewSessionArgs {
    #[serde(default, rename = "__harnx_parent_session_id")]
    parent_session_id: Option<String>,
}

#[derive(Deserialize)]
struct PromptArgs {
    message: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default, rename = "__harnx_parent_session_id")]
    parent_session_id: Option<String>,
}
```

### Additive TurnEvent Variant

New event type is purely additive, sibling to `HandoffRequested`:

```rust
// crates/harnx-core/src/event.rs:133-137
pub enum TurnEvent {
    // ...
    HandoffRequested {
        agent: String,
        session_id: Option<String>,
    },
    SubAgentStarted {
        agent: String,
        session_id: String,
    },
    Ended { outcome: TurnOutcome },
}
```

### Result Marker Shape

Results carry a structured `sub_agent` marker reusing `AgentSource`:

```rust
// crates/harnx-runtime/src/nats_worker/subagent_toolset.rs:303-315
fn turn_result_value(&self, result: &ThinClientTurnResult) -> Result<Value, ToolInvokeError> {
    let response = require_response(result)?;
    let source = AgentSource {
        agent: self.agent.clone(),
        session_id: Some(result.session_id.clone()),
        model: None,
    };
    Ok(json!({
        "session_id": result.session_id,
        "response": response,
        "sub_agent": source,
    }))
}
```

### Timeout + Cancel Plumbing

Built-in idle and operation timeouts with activity-based idle reset:

```rust
// crates/harnx-runtime/src/nats_worker/subagent_toolset.rs:154-194
loop {
    tokio::select! {
        result = &mut run_turn => { /* return result */ }
        _ = cancel.cancelled() => {
            let _ = cancel_tx.send(()).await;
            return Err(ToolInvokeError::Fatal("sub-agent tool call aborted".into()));
        }
        _ = &mut operation_timeout => {
            self.cancel_child(&child_session_id).await;
            return Err(ToolInvokeError::Recoverable("timed out (operation)".into()));
        }
        _ = &mut idle_timeout => {
            self.cancel_child(&child_session_id).await;
            return Err(ToolInvokeError::Recoverable("timed out (idle)".into()));
        }
        activity = activity_rx.changed() => {
            if activity.is_ok() {
                idle_timeout.as_mut().reset(tokio::time::Instant::now() + self.timeouts.idle);
            }
        }
    }
}
```

`ActivitySink` resets on every child event, avoiding the false-negative idle-timeout-during-active-turn bug from prior ACP implementation.

## Why This Works

1. **ThinClientSession preserves parent**: Unlike handoff, `ThinClientSession::run_turn` runs a child turn without touching the parent agent's session state. The parent remains leased and alive.

2. **Worker-agnostic dispatch**: The child turn publishes `SessionActivate` to the WorkQueue, any worker claims the lease and executes — no same-process constraint.

3. **Explicit publish for detached handlers**: Worker toolset-server handlers can't use `current_agent_event_sink()` (task-local from parent turn). Auto-injected `__harnx_parent_session_id` + direct publish to `sessions.{parent_id}.events` bridges the gap without coupling to engine internals.

4. **Additive event + marker reuse**: `TurnEvent::SubAgentStarted` is one new enum variant. Result marker reuses `AgentSource`. No new session-log types, no new subjects — UIs attach to existing `sessions.{id}.events`.

5. **Activity-aware timeouts**: `ActivitySink` watching child events resets idle deadline, correctly detecting inactivity without false positives during active child turns.

## Prevention Strategies

### Code Review Checklist

- [ ] Does nested agent delegation use `ThinClientSession`, not `_session_handoff`?
- [ ] For detached toolset handlers, does parent-event publishing use explicit `events_subject(parent_id)` rather than `current_agent_event_sink()`?
- [ ] Is `__harnx_parent_session_id` auto-injected for tools that need parent context?
- [ ] Are idle/operation timeouts wrapped around `run_turn` with activity-based reset?
- [ ] Does parent-abort cancellation propagate to child via `CancellationToken` → `cancel_rx` → `ControlCommand::Cancel`?

### Test Patterns

```rust
// Verify SubAgentStarted arrives before prompt result
let parent_events = subscribe_to_session_events(&parent_id);
let prompt_call = tokio::spawn(prompt_tool.invoke(args, cancel));
let event = parent_events.recv().await;
assert!(matches!(event, AgentEvent::Turn(TurnEvent::SubAgentStarted { .. })));
assert!(!prompt_call.is_finished());
let result = prompt_call.await?;
```

### Architectural Rules

1. **Handoff detaches the target**: `_session_handoff` durably queues an independent target session and finishes the source turn. `ThinClientSession` is the nesting primitive when the parent must await and consume a child result.

2. **In-engine vs toolset-server event path**: In-engine tool calls inherit `SCOPED_SINK`. Toolset-server handlers are detached — use explicit publish with auto-injected `__harnx_parent_session_id`.

3. **Worker-agnostic composition**: Child session activation + WorkQueue dispatch ensures any eligible worker can run the sub-agent turn.

4. **Idle timeout must be activity-aware**: Static timeouts produce false negatives when the child is active but slow. `ActivitySink` resets on child events.

## Known Follow-ups

- **WorkQueue consumer topology defect**: Multi-worker durable consumer setup with identical filter subjects fails on second worker startup (pre-existing, predates this migration, needs separate issue).
- **harnx-serve RPC flake under load**: Test `rpc_session_prompt_returns_ack_and_persists_effect` fails intermittently with 503 under concurrent load — readiness race in test harness, not a regression.
- **Non-blocking review findings**: Sanitized-name collision risk (agents with same sanitized name collide), silent `acp_servers/*.yaml` ignore after migration.

## Related Issues

- **GitHub:** [Issue #1224](https://github.com/dobesv/harnx/issues/1224)
- **Handoff architecture:** [feature-implementation/agent-handoff-architecture-2026-07-19.md](./agent-handoff-architecture-2026-07-19.md) — return-vs-continue pattern, session-id mapping
- **Idle timeout bug:** [async-patterns/acp-idle-timeout-false-negative-2026-06-18.md](../async-patterns/acp-idle-timeout-false-negative-2026-06-18.md) — prior timeout handling patterns
- **Scoped subscriptions:** [logic-errors/scoped-subscription-parallel-subagents-2026-05-01.md](../logic-errors/scoped-subscription-parallel-subagents-2026-05-01.md) — parallel sub-agent event streams
