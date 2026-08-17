---
title: "Agent handoff architecture: return-vs-continue pattern and caller-specific handling"
date: 2026-07-19
category: "feature-implementation"
problem_type: logic_error
component: "harnx-runtime, harnx-serve, harnx-ag-ui"
root_cause: "Inline agent-switching made per-frontend context handling impossible; session-id vs durable-id mismatch; Web UI live-vs-replay gating"
resolution_type: code_fix
severity: high
tags:
  - agent-handoff
  - session-management
  - nats-worker
  - ag-ui
  - return-vs-continue
  - session-id-mapping
  - web-ui-navigation
plan_ref: "harnx-issue-1091-agent-handoff"
last_updated: 2026-08-17
---

## Problem


## Symptoms

- **NATS handoff lost its persistence sink**: NATS-specific wiring (`FencedSessionLogSink`/`NatsSessionLogBackend`) existed only in pre-loop setup and was not re-run after handoff.
- **Web UI missed post-handoff events**: Serve outer loop continued delegated turn in-place on the original actor's task. Browser navigated to target session's SSE but target actor was idle — missed live events.
- **History replay caused ghost navigation**: `session_handoff` event in historical sessions triggered navigation even when no live run was active.
- **NATS handoff test failed**: Test read JetStream using human `Session.id` ("handoff-remote-session") but backend wrote to generated durable id ("alyQCQ").
- **Mock handoff tests didn't trigger handoff**: Setting `use_tools: <target>_session_handoff` only in-memory failed because NATS worker reloaded agent config from disk before executing tools.

## Investigation Steps

### 1. Oracle-recommended architecture (Option D)

Analyzed inline `continue` pattern in `run_agent_loop`. The loop called `Config::use_agent` then `continue`, leaving no opportunity for per-caller setup. Oracle recommended returning `LoopResult::HandoffRequested` and letting each caller own the outer loop and re-entry logic.

### 2. Serve vs TUI/CLI divergence

Serve sessions are URL-addressable and separately-subscribable. When handoff occurs, the browser must open a new SSE stream for the target session. If serve continues in-place, the target actor sits idle while the original actor broadcasts to a stream the browser has abandoned. Solution: serve finishes its run cleanly (`RUN_FINISHED`) and re-dispatches the handoff prompt to the target `SessionActor` via `SessionRegistry::get_or_spawn`.


### 3. Session-id vs durable-id mismatch

Traced `Config::use_agent("handoff-remote-session")` through session creation. Code at `config/session.rs:74-84` sets `Session.id = name` but for non-UUIDv7/non-timestamp names generates a separate `session_id`. NATS handoff path used `config.read().session.session_id` for lease/backend — correct. Test used the human name — wrong. Fix: read actual durable id from `config.read().session.as_ref().and_then(|s| s.session_id.clone())`.

### 4. Web UI live-vs-replay gate

Without a run-active flag, `session_handoff` events during history replay (MESSAGES_SNAPSHOT load) triggered navigation. Fix: track `isRunActive` boolean in `HarnxHttpAgent` — set `true` on `RUN_STARTED`, `false` on `RUN_FINISHED`/`RUN_ERROR`. Only invoke `onHandoff` callback when `isRunActive` is true.

### 5. Mock handoff test requirements

For a mock `call_fn` to trigger handoff: (1) source agent must have `use_tools: <target>_session_handoff` ON DISK (survives config reload); (2) target agent must exist on disk so `handoff_targets`/`allowed_tool_names` resolve; (3) mock returns `ToolCall { name: "<target>_session_handoff", arguments: { prompt: "..." } }`. In-memory-only config fails because NATS worker reloads before tool invocation.

## Root Cause

1. **Inline switching**: `run_agent_loop` owned the agent-switch and re-entry, preventing callers from injecting frontend-specific context (NATS lease/acquire, serve re-dispatch).
2. **Serve session model**: URL-addressable sessions require cross-actor dispatch, not in-place continuation.
3. **Session id duality**: Human `Session.id` ≠ generated durable `session_id` for non-UUIDv7 names. Persistence/NATS must use durable id.
4. **Replay lacks run state**: AG-UI history snapshot contains all events including past handoffs, but no run-liveness marker.

## Solution

### 1. `LoopResult` enum and return-vs-continue

```rust
// crates/harnx-runtime/src/agent_loop.rs
pub enum LoopResult {
    Completed,
    HandoffRequested {
        agent: String,
        session_id: Option<String>,
        prompt: String,
    },
}

pub async fn run_agent_loop(ctx: &AgentLoopContext, input: Input) -> Result<LoopResult> {
    // ... execution loop ...
    
    // On handoff tool call:
    harnx_core::sink::emit_agent_event(AgentEvent::Turn(TurnEvent::HandoffRequested {
        agent: switch.agent.clone(),
        session_id: switch.session_id.clone(),
    }));
    return Ok(LoopResult::HandoffRequested {
        agent: switch.agent,
        session_id: switch.session_id,
        prompt: switch.prompt,
    });
}
```

Each caller wraps in an outer loop:

```rust
loop {
    match run_agent_loop(&ctx, input).await? {
        LoopResult::Completed => break,
        LoopResult::HandoffRequested { agent, session_id, prompt } => {
            Config::use_agent(&config, &agent, session_id.as_deref(), abort).await?;
            input = from_str(&config, &prompt, None);
            continue;
        }
    }
}
```

### 2. NATS worker: re-acquire lease and backend

```rust
// crates/harnx-runtime/src/nats_worker/agent_loop.rs
fn run_agent_loop_segment(mut args: AgentLoopSegmentArgs) -> Pin<Box<dyn Future<Output = Result<()>>>> {
    Box::pin(async move {
        match run_agent_loop(&args.ctx, args.input).await? {
            LoopResult::Completed => Ok(()),
            LoopResult::HandoffRequested { agent, session_id, prompt } => {
                // Exit old session
                args.config.write().exit_agent()?;
                
                // Activate new agent/session
                Config::use_agent(&args.config, &agent, session_id.as_deref(), args.abort_signal.clone()).await?;
                
                // Read the generated durable session_id, NOT the human name
                let new_session_id = args.config.read().session.as_ref()
                    .and_then(|s| s.session_id.clone())
                    .context("NATS handoff did not establish new session")?;
                
                // Acquire new lease
                let new_lease = Arc::new(NatsSessionLease::acquire(NatsLeaseAcquireParams {
                    jetstream: args.jetstream_ctx.clone(),
                    session_id: &new_session_id,
                    worker_id: previous_lease.worker_id().to_string(),
                    generation: previous_lease.generation(),
                    config: args.lease_config.clone(),
                    session_index: args.session_index.clone(),
                }).await?);
                
                // Create new backend and event sink with correct session_id
                let new_backend = NatsSessionLogBackend::new(args.jetstream_ctx.clone(), &new_session_id);
                let new_event_sink = Arc::new(NatsEventSink::new(client, js, new_session_id.clone()).await);
                
                // Attach fenced sink
                attach_session_to_config(&args.config, new_session, &new_backend, Some(&new_lease));
                
                // Re-enter with new sink scope (events go to new session subject)
                args.input = from_str(&args.config, &prompt, None);
                harnx_core::sink::with_agent_event_sink(new_event_sink, async {
                    run_agent_loop_segment(args).await
                }).await
            }
        }
    })
}
```

### 3. Serve: re-dispatch to target actor

```rust
// crates/harnx-serve/src/session_actor.rs
Ok(LoopResult::HandoffRequested { agent, session_id, prompt }) => {
    done.sink.sink.close_text_segment();
    
    // Allocate session ID if None
    let target_session_id = session_id.unwrap_or_else(|| {
        prompt_config.write().new_session_id().expect("session ID allocation failed")
    });
    
    // Get or spawn target actor. Since 2026-08 this goes through the same
    // `get_or_spawn_in` helper as `SessionRegistry::get_or_spawn` (see
    // docs/solutions/async-patterns/session-actor-concurrency-invariants-2026-07-04.md §2) —
    // do not hand-roll the `registry.entry()` match here, it misses the closed-entry case.
    let target_key = SessionKey { agent: agent.clone(), session: target_session_id.clone() };
    let target_handle = self.get_or_spawn_target_session_actor(target_key.clone());
    
    // Re-dispatch prompt to target actor (original run finishes with RUN_FINISHED)
    let (reply_tx, _) = oneshot::channel();
    target_handle.tx.send(SessionCommand::Prompt {
        text: prompt,
        options: SessionPromptOptions::default(),
        reply: reply_tx,
    }).await.ok();
}
```

### 4. AG-UI: emit `session_handoff` custom event

```rust
// crates/harnx-serve/src/ag_ui.rs
AgentEvent::Turn(TurnEvent::HandoffRequested { agent, session_id }) => {
    self.emit_custom("turn_handoff_requested", json!({ "agent": agent, "session_id": session_id }));
    self.emit_custom("session_handoff", json!({ "agent": agent, "session_id": session_id }));
}
```

### 5. Web UI: live-run gate for navigation

```typescript
// web/src/ChatProvider.tsx
class HarnxHttpAgent extends HttpAgent {
  private isRunActive = false;
  
  async onEvent({ event }: { event: Event }) {
    if (event.type === 'RUN_STARTED') this.isRunActive = true;
    if (event.type === 'RUN_FINISHED' || event.type === 'RUN_ERROR') this.isRunActive = false;
    
    if (event.type === 'CUSTOM' && event.name === 'session_handoff' && this.isRunActive) {
      this.options.onHandoff?.(event.value.agent, event.value.session_id ?? null);
    }
  }
}
```

## Why This Works


2. **Serve re-dispatch**: Browser subscribes to target actor's SSE before the delegated run starts. Broadcast channel buffers early events; history snapshot on subscribe catches any missed events.

3. **Session-id distinction**: Durable `session_id` is the true NATS/persistence key. Human `Session.id` is for display/URLs. Tests must read `config.read().session.session_id`, not the handoff-provided name.

4. **Live-vs-replay gate**: `isRunActive` ensures navigation only on live handoff. History replay fires events but flag is `false`.

5. **Scope-based event routing**: `with_agent_event_sink(new_sink, async { ... })` ensures post-handoff advisory events go to the new session's NATS subject.

## Prevention Strategies

### Code Review Checklist

- [ ] Does agent-switching logic return or continue? Return if callers need context injection.
- [ ] Is `session_id` (durable) used for persistence/NATS? `Session.id` (human) for display/URL only?
- [ ] Does Web UI gate handoff navigation on run-active state?
- [ ] Does NATS handoff test read `config.read().session.session_id` for JetStream subject?
- [ ] Do mock handoff tests set `use_tools` ON DISK, not just in-memory?

### Test Patterns

```rust
// NATS handoff: read durable session_id
let actual_session_id = config.read().session.as_ref()
    .and_then(|s| s.session_id.clone())
    .expect("session should have durable id");
let backend = NatsSessionLog::new(js, &actual_session_id);
assert!(backend.load_events_blocking()?.len() > 0);
```

```rust
// Mock handoff: configure on disk
sandbox.write_agent("source", indoc! {r#"
    use_tools: target_session_handoff
"#});
sandbox.write_agent("target", "You are target.");

let call_fn: AgentCallFn = Arc::new(move |_input, _config, _abort| {
    Box::pin(async {
        Ok(("handoff".into(), None, vec![ToolCall::new(
            "target_session_handoff".into(),
            json!({ "prompt": "delegated work" }),
            None, None,
        )], CompletionTokenUsage::default()))
    })
});
```

### Architectural Rules

1. **Caller owns re-entry**: When frontend context differs (NATS vs file, serve vs TUI), the loop returns and callers handle continuation.
2. **Durable ID for persistence**: `Session.session_id` is the source of truth for NATS/JetStream paths. Human names are NOT durable.
3. **Test env requires companion bins**: `harnx-mock-mcp` must be built before `-p harnx-runtime` test runs that spawn MCP subprocesses.
4. **Config reload busts in-memory**: Agents execute from disk-reloaded config. Test setup must write to disk.

## Known Follow-ups

- **Headerless NATS sessions**: Handoff-created sessions lack agent/model metadata header
- **None-session_id navigation**: Web UI degrades gracefully when session_id is null
- **Outer-loop duplication**: Four frontends have similar outer-loop pattern — extraction candidate

## Related Issues

- **GitHub:** [Issue #1091](https://github.com/dobesv/harnx/issues/1091)
- **Prior solution:** [logic-errors/agent-switch-with-session-consistency-2026-05-03.md](../logic-errors/agent-switch-with-session-consistency-2026-05-03.md) — exit_agent before activation invariant
- **AG-UI protocol:** [integration-issues/ag-ui-server-protocol-integration-2026-07-04.md](../integration-issues/ag-ui-server-protocol-integration-2026-07-04.md) — raw session IDs, not UUID ThreadId
- **NATS index:** [integration-issues/nats-kv-session-index-enumeration-2026-06-27.md](../integration-issues/nats-kv-session-index-enumeration-2026-06-27.md) — session id handling
