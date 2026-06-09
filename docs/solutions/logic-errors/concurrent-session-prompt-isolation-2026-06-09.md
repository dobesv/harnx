---
title: "Isolating concurrent prompts in a single-connection ACP sub-agent server"
date: 2026-06-09
category: logic-errors
problem_type: logic_error
component: harnx-acp-server
root_cause: "shared global session state and process-global event sink mutated by concurrent prompts"
resolution_type: code_fix
severity: high
tags:
  - acp
  - concurrency
  - session-isolation
  - task-local
  - sub-agent
  - cancellation
plan_ref: concurrent-subagent-delegations
---

## Problem

Concurrent `*_session_prompt` delegations to the same ACP sub-agent server process corrupted session state, producing empty responses, 0-byte session log files, and orphaned `"tool response pending (results not yet persisted)"` placeholders. Two of three concurrent sub-agent session files were 0 bytes.

## Symptoms

```
# Parent agent dispatches concurrent prompts to same sub-agent:
#   urania_session_prompt [session-alpha]
#   urania_session_prompt [session-beta]
#   urania_session_prompt [session-gamma]

# Expected: each session log contains its own prompt + response
# Actual:
#   - session-alpha.yaml: correct content
#   - session-beta.yaml: 0 bytes
#   - session-gamma.yaml: 0 bytes
#   - responses: one correct, two empty strings
#   - orphaned placeholders: "tool response pending (results not yet persisted)"
```

- Frequency: 100% reproducible with 2+ concurrent prompts to same `HarnxAgent`
- Session logs (`.yaml` files) either empty or missing expected prompt text
- Streaming chunks lost; responses empty despite successful tool execution

## Investigation Steps

1. Traced `HarnxAgent::prompt` in `lib.rs`:
   - Each prompt called `config.exit_session()` + `config.use_session(Some(key))` on shared `GlobalConfig = Arc<RwLock<Config>>`
   - `Config.session: Option<Session>` is a single mutable slot
   - Concurrent prompt B switching `config.session` mid-loop corrupted prompt A's state

2. Found process-global event sink:
   - There is a single global `AGENT_EVENT_SINK`. `install_agent_event_sink`
     overwrites it with the new sink (and replays any buffered pending events to
     it); `clear_agent_event_sink` removes the installed sink and drops buffered
     events.
   - With one global slot, concurrent prompt B's `install` replaces the sink
     prompt A installed. From that point A's streaming chunks route to B's sink
     (or are dropped once B's prompt finishes and clears the slot) — so A's
     output is lost/misrouted.

3. Observed same-session race:
   - Even with per-prompt config forks, two prompts to the SAME `session_id` independently load/write the same on-disk `.yaml` transcript
   - Last-writer-wins clobber; one prompt's content lost

4. Hit stack overflow during fix:
   - `run_agent_loop` future is very large; inlined in `select!` arm overflowed thread stack before first poll
   - Looked like infinite recursion but wasn't (verified via zosimus)

## Root Cause

Two shared-state assumptions that only one prompt runs at a time per `HarnxAgent`:

1. **Shared active session**: `Config.session` is a single `Option<Session>` on `GlobalConfig`. Each prompt mutated it (`exit_session` + `use_session`). Concurrent prompts switched it out from under each other, causing wrong/no-session persistence.

2. **Process-global event sink**: `harnx_core::sink::install/clear_agent_event_sink` overwrote a single global sink. Concurrent prompts lost each other's streaming chunks.

3. **Same-session file race**: After fixing isolation, two concurrent prompts to SAME session_id still clobbered the shared `.yaml` file via independent loads/writes.

## Solution

### 1. Config::fork_session_scope()

Per-prompt config clone that shares Arc-backed resources but resets `session=None`:

```rust
// crates/harnx-runtime/src/config/mod.rs
pub fn fork_session_scope(&self) -> Config {
    Config {
        data: self.data.clone(),
        mcp_manager: self.mcp_manager.clone(),    // Arc — shared
        acp_manager: self.acp_manager.clone(),    // Arc — shared
        rag: self.rag.clone(),                    // Arc — shared
        model_cooldowns: self.model_cooldowns.clone(), // Arc — shared
        session: None,                            // ISOLATED — reset
        tui_before_editor: None,                  // Dropped — non-Clone
        tui_after_editor: None,                   // Dropped — non-Clone
        // ... other fields cloned or shared
    }
}
```

Each prompt runs against its own forked `GlobalConfig`:
```rust
let prompt_config: GlobalConfig = Arc::new(parking_lot::RwLock::new({
    let shared_config = self.config.read();
    shared_config.fork_session_scope()
}));
config.use_session(Some(&session_key))?;  // Mutates fork, not shared
```

### 2. Task-local event sink with nested-subagent propagation

```rust
// crates/harnx-core/src/sink.rs
tokio::task_local! {
    static SCOPED_SINK: Arc<dyn AgentEventSink>;
}

pub fn has_agent_event_sink() -> bool {
    // `AGENT_EVENT_SINK` is the existing process-global registry
    // (a `Mutex<SinkState>` whose `.sink` field holds the installed sink).
    SCOPED_SINK.try_with(|_| ()).is_ok() || AGENT_EVENT_SINK.lock().unwrap().sink.is_some()
}

pub async fn with_agent_event_sink<F: Future>(
    sink: Arc<dyn AgentEventSink>,
    fut: F,
) -> F::Output {
    SCOPED_SINK.scope(sink, fut).await
}

// Returns only the task-local sink (used to re-establish it across a
// `tokio::spawn` boundary). The global registry is the fallback inside
// `emit_agent_event_with_source`, not here.
pub fn current_agent_event_sink() -> Option<Arc<dyn AgentEventSink>> {
    SCOPED_SINK.try_with(|sink| sink.clone()).ok()
}
```

Each prompt wraps its agent loop:
```rust
// crates/harnx-acp-server/src/lib.rs
let loop_result = harnx_core::sink::with_agent_event_sink(sink, async {
    let mut run_loop = Box::pin(harnx_runtime::run_agent_loop(&loop_ctx, input));
    tokio::select! {
        r = &mut run_loop => Some(r),
        _ = grace_cancel => None,
    }
}).await;
```

**Pitfall**: task-locals don't cross `tokio::spawn`. Nested-sub-agent chunk forwarder must re-scope:

```rust
// crates/harnx-acp/src/manager.rs
let captured_sink = harnx_core::sink::current_agent_event_sink();
let forward_handle = if let Some(sink) = captured_sink {
    tokio::spawn(async move {
        harnx_core::sink::with_agent_event_sink(
            sink,
            forward_acp_chunks(chunk_rx, spinner, msg),
        ).await
    })
} else {
    tokio::spawn(forward_acp_chunks(chunk_rx, spinner, msg))
};
```

### 3. Per-session lock for same-session serialization

```rust
// crates/harnx-acp-server/src/lib.rs
struct HarnxSession {
    abort_signal: AbortSignalInner,
    cancel_notify: Arc<tokio::sync::Notify>,
    /// Serializes prompts targeting THIS session.
    /// Different sessions run fully concurrently (own locks).
    prompt_lock: Arc<tokio::sync::Mutex<()>>,
}
```

Lock acquisition pattern:
```rust
let (abort_signal, cancel_notify, prompt_lock) = {
    let sessions = self.sessions.lock().await;
    let session = sessions.get(session_key.as_str())?;
    (session.abort_signal.clone(), session.cancel_notify.clone(), session.prompt_lock.clone())
};  // RELEASE sessions map lock FIRST

let _prompt_guard = prompt_lock.lock_owned().await;  // THEN await per-session lock
abort_signal.reset();  // RESET signal AFTER acquiring lock, not before
```

**Ordering critical**: if `abort_signal.reset()` runs before acquiring `prompt_lock`, a queued same-session prompt can clear a cancellation the running prompt hasn't observed.

### 4. Box::pin the agent loop future

```rust
// Embedding run_agent_loop inline in select! overflows stack
let mut run_loop = Box::pin(harnx_runtime::run_agent_loop(&loop_ctx, input));
tokio::select! {
    r = &mut run_loop => Some(r),
    _ = grace_cancel => None,
}
```

## Why This Works

- **Forked config**: Each prompt owns its session slot; no cross-prompt mutation
- **Task-local sink**: Each prompt's task tree resolves its own sink; chunks never lost to concurrent installs
- **Nested-subagent re-scoping**: Events from nested ACP calls route through parent's scoped sink
- **Per-session lock**: Same-session prompts serialize on-disk writes; different sessions stay concurrent
- **Cancellation ordering**: Abort signal reset only after lock acquisition preserves cancel semantics

## Prevention Strategies

**Test Cases:**
- `test_concurrent_prompts_isolate_session_scope_and_sink`: 3 concurrent prompts to DIFFERENT sessions; assert non-empty logs, own prompt text, no placeholders
- `test_same_session_concurrent_prompts_do_not_clobber_log`: 2 concurrent prompts to SAME session_id; assert BOTH prompts survive in single `.yaml` log

**Testing mechanics:**
- Use `#[tokio::test(flavor = "multi_thread")]` + inner `LocalSet::run_until` (prompt path uses `spawn_local`)
- Session logs are `.yaml`, not `.md`
- Use `MockClient`/`TestStateGuard` + `sessions_dir_override` for deterministic, isolated runs

**Code Review Checklist:**
- [ ] Does shared state assume single-threaded access?
- [ ] Do task-locals cross `tokio::spawn` boundaries?
- [ ] Are locks acquired in consistent order (map lock released before per-session lock awaited)?
- [ ] Is cancellation reset after lock acquisition, not before?
- [ ] Are large futures `Box::pin`ned before racing in `select!`?

## Related Issues

- Issue #783 — concurrent delegations to same sub-agent
- [scoped-subscription-parallel-subagents-2026-05-01.md](./scoped-subscription-parallel-subagents-2026-05-01.md) — related ACP concurrency issue (different root cause)
