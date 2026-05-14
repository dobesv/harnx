---
title: "Hook mutation for tool input and response replacement"
date: 2026-05-14
category: logic-errors
problem_type: logic_error
component: harnx-hooks
root_cause: "Hook system could only observe and control tool calls, not mutate their arguments or responses"
resolution_type: code_fix
severity: medium
tags:
  - hooks
  - mutation
  - tool-execution
  - chaining
  - persistent-hooks
plan_ref: harnx-533-hook-mutation
---

## Problem

Hooks could observe tool calls and block/approve them, but could not mutate the tool arguments or responses. Users could not inject environment variables, rewrite paths, redact sensitive data, or otherwise modify what tools receive or return.

## Symptoms

- `PreToolUse` hooks could deny/allow but not modify `tool_input`
- `PostToolUse` hooks could observe `tool_response` but not alter it
- Multiple hooks could not compose mutations (e.g., one injects AWS creds, another adds GitHub proxy settings)
- Persistent hooks (`claude-command-persistent`) never participated in mutation semantics

## Investigation Steps

1. Identified that `HookSpecificOutput` only had `permissionDecision` / `permissionDecisionReason` — no mutation fields
2. `dispatch_hooks_with_count_and_manager` tracked `additional_context` across hooks but not `tool_input`/`tool_response`
3. `persistent.rs::send_event` hardcoded `HookResultControl::Continue`, ignoring any permission decision from the hook output
4. `eval_tool_calls` discarded `PostToolUse` outcome entirely (`let _ = ...`)
5. Found that extraction from `hook_specific_output` must happen BEFORE the `match control` arms, otherwise a hook returning both mutation AND Ask/Block would lose the mutation

## Root Cause

1. **Missing mutation fields**: `HookSpecificOutput` and `HookResult` had no fields to carry mutated `tool_input`/`tool_response`
2. **No chaining logic**: Dispatch loop didn't track or propagate mutations across hooks
3. **Persistent hook parity gap**: `persistent.rs` ignored `hookSpecificOutput.permissionDecision`, always returning `Continue`
4. **Engine discard**: `eval_tool_calls` didn't read mutation fields from hook outcomes
5. **Ask timing issue**: Mutation was applied after Ask confirmation check, so users saw original args in confirm dialog

## Solution

### 1. Added mutation fields to core types (`harnx-core/src/hooks.rs`)

```rust
// HookSpecificOutput — what hooks return
#[serde(default)]
pub tool_input: Option<Value>,      // JSON key: toolInput
#[serde(default)]
pub tool_response: Option<Value>,   // JSON key: toolResponse

// HookResult — dispatcher aggregate
#[serde(default)]
pub mutated_tool_input: Option<Value>,      // engine reads this
#[serde(default)]
pub mutated_tool_response: Option<Value>,   // engine reads this
```

**Naming rationale**: `tool_input`/`tool_response` in `HookSpecificOutput` = wire protocol from hooks; `mutated_tool_input`/`mutated_tool_response` in `HookResult` = dispatcher-internal aggregate for engine consumption.

### 2. Chaining logic in dispatch (`harnx-hooks/src/dispatch.rs`)

Track mutations across hooks and patch payload for each subsequent hook:

```rust
let mut current_tool_input: Option<Value> = None;
let mut current_tool_response: Option<Value> = None;

for hook in hooks {
    // Patch payload so next hook sees already-mutated value
    patch_payload_mutation(
        &mut payload.hook_event,
        current_tool_input.as_ref(),
        current_tool_response.as_ref(),
    );

    // ... dispatch hook ...

    // CRITICAL: Extract mutations BEFORE match control arms
    if let Some(ref hso) = result.hook_specific_output {
        if let Some(ref ti) = hso.tool_input {
            current_tool_input = Some(ti.clone());
        }
        if let Some(ref tr) = hso.tool_response {
            current_tool_response = Some(tr.clone());
        }
    }

    match control {
        HookResultControl::Block { reason } => {
            return HookOutcome {
                control: HookResultControl::Block { reason },
                result: HookResult {
                    mutated_tool_input: current_tool_input,
                    mutated_tool_response: current_tool_response,
                    ..result
                },
            };
        }
        // ... Ask and Continue arms also carry mutations ...
    }
}
```

**`patch_payload_mutation`** mutates `HookEvent::PreToolUse.tool_input` and `HookEvent::PostToolUse.tool_input`/`tool_response` so each hook receives the current value.

### 3. Extracted shared control derivation (`harnx-hooks/src/executor.rs`)

```rust
/// Derive HookResultControl from HookResult based on
/// hookSpecificOutput.permissionDecision. Used by both
/// one-shot and persistent hooks.
pub fn control_from_result(result: &HookResult) -> HookResultControl {
    match result
        .hook_specific_output
        .as_ref()
        .and_then(|output| output.permission_decision.as_deref())
    {
        Some("deny") => HookResultControl::Block { ... },
        Some("ask") => HookResultControl::Ask { ... },
        _ => HookResultControl::Continue,
    }
}
```

### 4. Fixed persistent hooks (`harnx-hooks/src/persistent.rs`)

```rust
// Before: hardcoded Continue
Ok(HookOutcome {
    control: HookResultControl::Continue,
    result,
})

// After: use shared control derivation
let control = control_from_result(&result);
Ok(HookOutcome { control, result })
```

### 5. Applied mutations in engine (`harnx-engine/src/tool.rs`)

**PreToolUse mutation BEFORE Ask check** (critical ordering):

```rust
let pre_outcome = (ctx.dispatch_hook_fn)(pre_event).await;

// CRITICAL: Apply mutation BEFORE Ask check so user sees actual args
let (json_data, tool_input) = if let Some(mutated) = pre_outcome.result.mutated_tool_input {
    (mutated.clone(), mutated)
} else {
    (json_data, tool_input)
};

if let HookResultControl::Ask { reason } = pre_outcome.control {
    // User now sees mutated args in confirmation prompt
    if !(ctx.confirm_tool_use_fn)(&call.name, &json_data, reason.as_deref()) { ... }
}
```

**PostToolUse mutation** (previously discarded):

```rust
let post_outcome = (ctx.dispatch_hook_fn)(post_event).await;
if let Some(mutated_response) = post_outcome.result.mutated_tool_response {
    result = mutated_response;
}
```

## Why This Works

1. **Chaining**: Each hook receives `tool_input`/`tool_response` patched with prior mutations, so they compose
2. **Extraction before match**: Same hook can mutate AND return Ask/Block — mutation extracted before control branching
3. **Mutation before Ask**: User confirmation shows the actual args that will be dispatched
4. **Persistent parity**: `control_from_result` shared function ensures one-shot and persistent hooks have identical control semantics
5. **Field naming clarity**: Wire protocol fields (`toolInput`/`toolResponse`) separate from engine aggregate fields (`mutatedToolInput`/`mutatedToolResponse`)

## Prevention Strategies

**Test Cases:**
- Single `PreToolUse` hook returning `toolInput` mutation → outcome carries mutated value
- Two `PreToolUse` hooks → second receives mutated payload from first; final outcome is second's mutation
- Hook that mutates AND returns Ask/Block → both mutation and control preserved
- Persistent hook returning `permissionDecision: "deny"` → actually blocks
- Mutation chain with one hook returning bad JSON → falls back to Continue with no mutation

**Code Review Checklist:**
- [ ] Mutation extraction happens BEFORE `match control` arms in dispatch loop
- [ ] Ask check happens AFTER mutation is applied to `json_data`
- [ ] `patch_payload_mutation` is called before each hook execution
- [ ] Persistent hooks call `control_from_result`, not hardcoded `Continue`
- [ ] `HookResult.mutated_tool_*` fields set in all control branches (Block/Ask/Continue)

**Best Practices:**
- Use `patch_payload_mutation` to ensure chaining semantics
- Test multi-hook scenarios, not just single hook
- Verify mutation+Ask/Block combination works correctly

## Related Issues

- **GitHub Issue:** [#533 — Mutable tool call args and responses](https://github.com/dobesv/harnx/issues/533)
- **Related Solution:** [api-design/per-call-env-param-bash-mcp-2026-05-13.md](../api-design/per-call-env-param-bash-mcp-2026-05-13.md) — Prior art for env injection that hooks mutation enables
- **Documentation:** [docs/hooks-guide.md](../../hooks-guide.md) — Comprehensive hooks reference with mutation examples
