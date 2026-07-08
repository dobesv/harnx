---
title: "AG-UI interrupt/resume for tool-approval HITL (Design B)"
date: 2026-07-08
category: "integration-issues"
problem_type: integration_issue
component: "harnx-serve, harnx-runtime, harnx-engine"
root_cause: "Synchronous engine approval seam cannot implement async human-scale approval; provider API contracts require one result per emitted tool call"
resolution_type: code_fix
severity: high
tags:
  - ag-ui
  - hitl
  - tool-approval
  - interrupt
  - resume
  - assistant-ui
plan_ref: "harnx-webui-parity"
---

# AG-UI interrupt/resume for tool-approval HITL (Design B)

## Problem

The harnx engine's tool-approval seam is a synchronous `ConfirmToolUseFn` returning `bool` — it cannot implement async human-in-the-loop approval requiring UI interaction. Using a blocking oneshot would deadlock the actor loop. Additionally, LLM provider APIs (OpenAI, Anthropic) require exactly one result per emitted tool call — a subset response violates the contract.

## Symptoms

```
# Before tri-state:
ConfirmToolUseFn returns bool → no way to signal "ask human"
Blocking oneshot in actor → deadlock when awaiting response
Tool results for subset of calls → "tool response lost" errors on resume

# Wrong Design A approach:
TOOL_APPROVAL_REQUESTED event + blocking wait → requires custom client logic
Client must implement bespoke handshake → not using stock @assistant-ui/react-ag-ui

# Round preservation bugs:
Resuming with partial tool results → "tool call without corresponding result" API errors
Auto-approved calls missing from PendingInterruptBatch → re-interrupt loop
Empty text on resume → "prompt required" error
Attachments lost across interrupt/resume → "cid: refs not found"
Promptless SSE join returning empty MESSAGES_SNAPSHOT → stale cached snapshot
```

## Investigation Steps

1. Reviewed `@assistant-ui/react-ag-ui@0.0.44` — confirmed stock interrupt lifecycle support via `parseRunFinishedOutcome`, `submitInterruptResponses`, `useAgUiInterrupts`.
2. Traced engine's `eval_tool_calls` — synchronous callback cannot defer without blocking.
3. Analyzed OpenAI/Anthropic API contracts — tool_calls require matching tool_results.
4. Found `execute_tool_round_with_persistence(REUSE_EXISTING_CALLS)` — allows filling slots in existing pending ToolCalls.
5. Identified that `session_actor.rs` cached empty snapshot at actor spawn — refresh needed on Subscribe.
6. Discovered mixed-round bug: resume confirm-override resolved auto-approved calls to Defer instead of Approve.

## Root Cause

### 1. Synchronous approval seam

`ConfirmToolUseFn` is `Arc<dyn Fn(&ToolCall, &Value, Option<&str>) -> ToolUseConfirmation + Send + Sync>`. Called synchronously during `eval_tool_calls`. Cannot await UI interaction.

### 2. Provider API contract

When the LLM emits 5 tool calls, the next turn MUST include 5 tool results. Persisting only deferred calls loses auto-approved call results → API error.

### 3. Wrong execution mode on resume

Appending new ToolCalls instead of reusing existing pending calls orphans results. The slot indices don't match.

### 4. Mixed-round decision logic

Resume decisions map via `interrupt_id`. Auto-approved calls (not in `interrupts` array) were incorrectly defaulting to Defer because they weren't in the decision map.

## Solution

### 1. Tri-state `ToolUseConfirmation`

```rust
// crates/harnx-engine/src/tool.rs

pub enum ToolUseConfirmation {
    Approve,                    // Execute immediately
    Deny { reason: Option<String> }, // Return blocked result
    Defer,                      // Collect for batch interrupt
}
```

Engine collects `Defer` calls, returns `ToolApprovalRequiredError` BEFORE dispatching any approved tools (defer-before-dispatch).

### 2. Typed error with full-round preservation

```rust
pub struct ToolApprovalRequiredError {
    tool_calls: Vec<ToolCall>,        // ALL emitted calls (auto-approved + deferred)
    deferred_calls: Vec<DeferredToolCall>,
}
```

Server persists `PendingInterruptBatch` with complete `tool_calls` array.

### 3. RUN_FINISHED with interrupt outcome

```rust
// Server ends run with terminal outcome (NOT RUN_ERROR)
Event::RunFinished(RunFinishedEvent {
    result: Some(json!({
        "outcome": {
            "type": "interrupt",
            "interrupts": [
                {"id": "call-123", "toolCallId": "call-123", "reason": "tool_call", "message": "...", "responseSchema": {...}}
            ]
        }
    })),
})
```

Server returns to `Idle` state — no blocking wait.

### 4. Runtime continuation seam

```rust
// crates/harnx-runtime/src/agent_loop.rs

pub async fn continue_agent_loop_from_tool_round(
    config: GlobalConfig,
    input: Input,
    tool_calls: Vec<ToolCall>,      // Persisted from interrupt
    decisions: Vec<ToolApprovalDecision>,
    ...
) -> Result<()>
```

Resumes from persisted pending ToolCalls with preseeded decisions WITHOUT re-asking the model.

### 5. Persistence fill-in-place

```rust
// crates/harnx-runtime/src/tool.rs

pub struct PersistenceMode {
    mode: bool,  // REUSE_EXISTING_CALLS = true
}

// On resume:
execute_tool_round_with_persistence(REUSE_EXISTING_CALLS)
// Results fill existing pending ToolCalls slots via add_tool_results
// NOT appending duplicate ToolCalls entry
```

### 6. Mixed-round resume logic

```rust
// crates/harnx-serve/src/session_actor.rs

fn build_resume_continuation(&self, resume: &[InterruptResume]) -> Option<ResumeContinuation> {
    let pending = &self.pending_interrupt_batch?;
    
    // Resolve auto-approved calls to Approve
    // Only genuinely-pending-with-missing-decision → Defer
    let pending_ids: HashSet<_> = pending.interrupts.iter()
        .map(|i| i.tool_call_id.as_str())
        .collect();
    
    let decisions: Vec<_> = pending.tool_calls.iter()
        .map(|call| {
            if pending_ids.contains(call.id.as_str()) {
                // Find user decision for pending interrupt
                resume.iter()
                    .find(|r| r.interrupt_id == call.id)
                    .map(|r| ToolApprovalDecision {
                        tool_call_id: call.id.clone(),
                        approved: r.payload.approved && matches!(r.status, InterruptResumeStatus::Approved),
                        reason: r.payload.reason.clone(),
                    })
                    .unwrap_or(ToolApprovalDecision {
                        tool_call_id: call.id.clone(),
                        approved: false,  // Missing decision = deny
                        reason: Some("missing decision".into()),
                    })
            } else {
                // Auto-approved call → Approve
                ToolApprovalDecision {
                    tool_call_id: call.id.clone(),
                    approved: true,
                    reason: None,
                }
            }
        })
        .collect();
    
    Some(ResumeContinuation { pending: pending.clone(), decisions })
}
```

### 7. Empty-text resume

Server must accept empty `text` on resume and use stored pending batch text:
```rust
// crates/harnx-serve/src/session_actor.rs
let text = if resume.is_empty() { text } else { pending.text.clone() };
```

### 8. attachment_refs through interrupt/resume

```rust
pub struct PendingInterruptBatch {
    pub attachment_refs: Vec<String>,  // Store from original prompt
    // ...
}

// On resume:
input.set_attachment_refs(resume.pending.attachment_refs.clone());
```

### 9. Restore-on-open

```rust
// crates/harnx-serve/src/session_actor.rs
SessionCommand::Subscribe { reply } => {
    self.refresh_history_snapshot();  // LOAD from disk BEFORE replying
    let _ = reply.send(SubscribeResult {
        snapshot: self.history_snapshot.clone(),
        events: self.broadcast_tx.subscribe(),
    });
}
```

Previously: snapshot cached empty at actor spawn → Subscribe returned empty MESSAGES_SNAPSHOT.

## Why This Works

1. **Tri-state enables defer**: `Defer` signals "ask human" without blocking. Engine collects all defers, returns error after evaluating all calls.

2. **Defer-before-dispatch**: No tool executes when interrupted. All `Approve` calls are queued but not dispatched. On resume, decisions are applied BEFORE execution.

3. **Full-round preservation**: Provider API receives complete tool_results array matching emitted calls. No "missing result" errors.

4. **Persistence fill-in-place**: `REUSE_EXISTING_CALLS` ensures results fill the correct slots in the interrupted ToolCalls message. No orphaned results.

5. **Stock AG-UI lifecycle**: Design B uses `@assistant-ui/react-ag-ui` built-in `interrupt`/`resume` handling. No custom client handshake.

6. **Idle state after interrupt**: Server can accept new connections, serve snapshots, and queue resume requests. No blocked actor loop.

## Prevention Strategies

### Test Cases

```rust
// crates/harnx-serve/src/session_actor.rs (tests)

// 1. Full-round preservation
session_actor_resume_with_persisted_tool_calls_includes_all_calls_not_just_deferred()
// - LLM emits 5 calls, 3 auto-approved, 2 deferred
// - PendingInterruptBatch.tool_calls must have ALL 5

// 2. Mixed-round resume (critical!)
session_actor_resume_with_mixed_round_approves_non_pending_calls_without_reinterrupt()
// - Calls: auto-approved-A, deferred-B, auto-approved-C, deferred-D
// - Resume with decisions for B and D
// - A and C automatically approved, B and D use user decisions
// - Round completes in ONE resume (not re-interrupt loop)

// 3. Missing decision for pending tool
session_actor_resume_missing_decision_for_pending_tool_causes_reinterrupt()
// - Two pending tools, only one decision provided
// - Second tool re-interrupts

// 4. Attachment refs survive interrupt/resume
session_actor_resume_preserves_attachment_refs()
// - Prompt with cid: attachment, interrupt, resume
// - build_input gets attachment_refs from pending batch

// 5. Empty text on resume
// - Resume with "" text uses stored pending.text
```

### Handler generic over Body type

```rust
// crates/harnx-serve/src/lib.rs

// BAD: Reimplementing logic in test
#[cfg(test)]
async fn upload_handler(body: Vec<u8>) -> Response<Body> {
    // Duplicates production logic → worthless tests
}

// GOOD: Generic handler
async fn upload_handler<B>(body: B) -> Response<Body>
where
    B: hyper::body::Body,
{
    // Tests invoke REAL handler with mock body
}
```

### Testing pitfalls to avoid

1. **Seeding with matching text**: Interrupt/resume tests that use identical text for interrupt and resume create "success-path illusion" — empty-text bugs hidden.
2. **Handler reimplementation**: Tests duplicating handler logic don't test the actual production code path.
3. **Mock state isolation**: Each test needs `TestStateGuard::new(None)` to reset global state.

### Code Review Checklist

- [ ] Does `PendingInterruptBatch.tool_calls` include ALL calls (auto-approved + deferred)?
- [ ] Does resume resolve auto-approved calls to `Approve`?
- [ ] Does resume use `REUSE_EXISTING_CALLS` mode?
- [ ] Does Subscribe refresh history snapshot before replying?
- [ ] Does resume accept empty text and use stored pending batch text?
- [ ] Are attachment_refs threaded through build_input on resume?
- [ ] Does the handler under test use a generic Body type (not reimplementation)?

## Related Issues

- **Plan:** `harnx-webui-parity` (issue #959)
- **Related Solution:** [attachment-upload-by-reference.md](../attachment-upload-by-reference.md) — Attachment cache and cid: refs
- **Related Solution:** [ag-ui-server-protocol-integration-2026-07-04.md](./ag-ui-server-protocol-integration-2026-07-04.md) — AG-UI protocol basics
- **Related Solution:** [ag-ui-client-conformance-stock-adapter-2026-07-07.md](./ag-ui-client-conformance-stock-adapter-2026-07-07.md) — Stock assistant-ui adapter

## File Pointers

- `crates/harnx-engine/src/tool.rs`: `ToolUseConfirmation` tri-state, `ToolApprovalRequiredError`, `eval_tool_calls` defer-before-dispatch
- `crates/harnx-runtime/src/agent_loop.rs`: `continue_agent_loop_from_tool_round` runtime continuation
- `crates/harnx-runtime/src/tool.rs`: `PersistenceMode::REUSE_EXISTING_CALLS`, `execute_tool_round_with_persistence`
- `crates/harnx-runtime/src/config/session.rs`: Input `set_attachment_refs`, attachment expansion
- `crates/harnx-serve/src/session_actor.rs`: `PendingInterruptBatch`, `SessionState::Interrupted`, `build_resume_continuation`, `refresh_history_snapshot`, mixed-round tests
- `crates/harnx-serve/src/ag_ui_rpc.rs`: `InterruptResume` RPC types, resume validation
- `crates/harnx-serve/src/lib.rs`: Upload endpoint, promptless snapshot refresh
- `web/src/ChatProvider.tsx`: Client-side interrupt handling with `resume` transformation
