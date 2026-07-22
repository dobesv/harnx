---
title: "OpenAI /v1/responses API support for reasoning models with function tools"
date: 2026-07-22
category: integration-issues
problem_type: integration_issue
component: harnx-client
root_cause: API endpoint mismatch — reasoning models with tools require different wire format
resolution_type: code_fix
severity: high
tags:
  - openai
  - responses
  - reasoning
  - model-alias
plan_ref: openai-responses-api
---

# OpenAI Responses API Integration

## Problem

OpenAI's `/v1/chat/completions` endpoint rejects requests combining function tools with `reasoning_effort` for GPT-5.6 models:

```
Function tools with reasoning_effort are not supported for gpt-5.6-terra in /v1/chat/completions.
To use function tools, use /v1/responses or set reasoning_effort to 'none'.
```

The `/v1/responses` endpoint uses a completely different wire shape:
- `input[]` array with typed items instead of `messages[]`
- Top-level `instructions` for system prompt
- Flat `tools[]` without nested `function` wrapper
- `max_output_tokens` (not `max_tokens`) with minimum floor of 16
- Reasoning via `encrypted_content` for stateless replay across tool turns
- `store: false` default (vs server default `true`)

## Solution

### Dispatch by Endpoint

New `endpoint: Option<String>` field on `ModelData` triggers routing. Model aliases declare `endpoint: responses` in `models.yaml`:

```yaml
- name: gpt-5.6-sol:high
  real_name: gpt-5.6-sol
  endpoint: responses
  patches:
    - del(.body.temperature) | del(.body.top_p) | .body.reasoning.effort = "high"
```

Dispatch in `openai.rs` checks `model.endpoint()`:
- `Some("responses")` → `/v1/responses` URL + `openai_build_responses_body`
- `_` (default) → `/v1/chat/completions` + existing chat body builder

**Key design constraint**: Shared `openai_build_chat_completions_body` and `openai_extract_chat_completions` functions remain untouched. They're used by `openai_compatible.rs` and `azure_openai.rs`. The `impl_client_trait!` macro is unchanged.

### Wire Format Transformation

`openai_build_responses_body` transforms `ChatCompletionsData`:

```rust
pub fn openai_build_responses_body(data: ChatCompletionsData, model: &Model) -> Value {
    json!({
        "model": model.real_name(),
        "instructions": system_prompt,  // extracted from leading system message
        "input": input_items,           // typed items (message, function_call, function_call_output, reasoning)
        "tools": flat_tools,            // no nested function wrapper
        "max_output_tokens": v.max(16), // floor enforced
        "store": false,                 // hard default
        "include": ["reasoning.encrypted_content"],
    })
}
```

Input item types: `message` (user/assistant content), `function_call`, `function_call_output`, `reasoning`.

### Reasoning Replay via `thought_signature`

Mirrors Gemini/Claude pattern using existing `ToolCall.thought_signature` field:

1. **Extract**: On response, capture reasoning item's `encrypted_content`, attach to following `function_call` items as `thought_signature`
2. **Persist**: `ToolCall.thought_signature` already round-trips through session save/reload (no core/runtime changes needed)
3. **Replay**: Next turn, builder emits `{type: "reasoning", encrypted_content, summary: []}` as input item before `function_call` items

**Extraction location**: `openai_extract_responses` for non-streaming, `openai_handle_responses_event` for streaming.

### SSE Streaming Handler

`openai_responses_streaming` handles typed SSE events:

| Event | Action |
|-------|--------|
| `response.output_text.delta` | `handler.text(delta)` |
| `response.reasoning_summary_text.delta` | `handler.thought(delta)` |
| `response.function_call_arguments.delta` | Append to pending arguments |
| `response.function_call_arguments.done` | Finalize pending tool call |
| `response.output_item.done` | Capture reasoning `encrypted_content` |
| `response.completed` | Emit usage + attach late signatures |
| `response.failed` / `response.error` | `catch_error` |

Usage path: `data["response"]["usage"]` (nested under `response`, not top-level).

### Privacy Default

`store: false` prevents OpenAI dashboard logging. OpenAI retains payloads ~30 days for abuse monitoring unless the account has Zero Data Retention (ZDR).

Override via model patches (`.body.store = true`) or client-config `patches.responses`.

## Key Decisions

1. **Static endpoint routing** — Model alias declares `endpoint: responses`. No runtime suffix parsing, no auto-detection. Explicit and auditable.

2. **Isolated module** — `openai_responses.rs` contains all Responses-specific logic. No cross-provider coupling.

3. **Patch schema extension** — `RequestPatches.responses` field + `ModelType::extract_patches_for(patches, endpoint)` selects endpoint-appropriate patches.

4. **Reasoning on `thought_signature`** — Reuses existing Gemini/Claude/Bedrock field. No new core data model.

5. **Reasoning-level aliases** — `:high` and `:max` variants (gpt-5.6 supports through `max`). Patches set `.body.reasoning.effort`.

## Gotchas

### 1. Streaming Event Ordering

**Problem**: `response.function_call_arguments.done` may arrive BEFORE the reasoning item's `response.output_item.done` containing `encrypted_content`. If tool call is emitted immediately with `state.encrypted_content.take()`, signature is `None`.

**Fix**: Defer signature attachment until `response.completed`. Added `SseHandler::attach_thought_signature_to_pending_tool_calls(signature)` which iterates all pending tool calls and attaches the signature to any with `thought_signature: None`. Encrypted content captured whenever it arrives and applied at stream end.

**Unit tests**: `test_streaming_normal_order_reasoning_before_tool` and `test_streaming_reversed_order_tool_before_reasoning`.

### 2. Multi-Tool Non-Streaming Clone vs Take

**Problem**: Non-streaming `openai_extract_responses` originally used `pending_encrypted_content.take()` which consumes on first `function_call`. In a response with multiple tool calls after a reasoning item, only the first received the signature.

**Fix**: Changed to `pending_encrypted_content.as_ref().cloned()` so ALL function_calls in the turn inherit the reasoning signature, matching streaming behavior and Bedrock/Claude precedent.

**Unit test**: `test_extract_multi_tool_call_response_shares_thought_signature` verifies both tool calls receive shared `thought_signature`.

### 3. `max_output_tokens` Floor

Responses API rejects values < 16. Body builder clamps: `v.max(16)`. Chat/completions has no such floor.

### 4. Test B Streaming vs Non-Streaming

Initial e2e test ran streaming against mock returning plain JSON (not SSE). Mock handler ignores `stream: true` flag. Fix: Test B config explicitly sets `stream: false` to exercise non-streaming path.

## Files Touched

| File | Change |
|------|--------|
| `crates/harnx-core/src/model.rs` | `ModelData::endpoint`, `RequestPatches::responses`, `extract_patches_for` |
| `crates/harnx-client/src/lib.rs` | Register `pub(crate) mod openai_responses;` |
| `crates/harnx-client/src/openai.rs` | Dispatch by `model.endpoint()` in prepare, completions, and streaming |
| `crates/harnx-client/src/openai_responses.rs` | Body builder, extract, streaming, reasoning replay |
| `crates/harnx-client/src/stream.rs` | `attach_thought_signature_to_pending_tool_calls` on `SseHandler` |
| `crates/harnx/models.yaml` | `gpt-5.6-sol:high|max`, `gpt-5.6-terra:high|max` aliases |
| `crates/harnx/src/test_utils/mock_openai_server.rs` | `/v1/responses` route, `__path` logging, `encrypted_content` support |
| `crates/harnx/tests/openai_responses_e2e.rs` | E2E tests: endpoint hit, reasoning replay, gpt-4o regression guard |
| `example_config/clients/openai.yaml` | Documented `patches.responses` + privacy note |

## Related Issues

- **GitHub Issue**: [#1138](https://github.com/dobesv/harnx/issues/1138)
- **Prior Art**: Gemini/Vertex `thoughtSignature` handling (`vertexai.rs:463`), Claude (`claude.rs:468-470`), Bedrock (`bedrock.rs:555-557`)
