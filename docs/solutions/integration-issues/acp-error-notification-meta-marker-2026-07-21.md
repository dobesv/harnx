---
title: "ACP: Forward model errors via harnx:error meta marker to prevent response_text pollution"
date: 2026-07-21
category: "integration-issues"
problem_type: integration_issue
component: "harnx-acp, harnx-acp-server"
root_cause: "ACP SessionUpdate enum lacks dedicated error variant; errors forwarded as plain text accumulated into parent agent's response_text"
resolution_type: code_fix
severity: medium
tags:
  - acp
  - error-handling
  - meta-marker
  - sub-agent
  - protocol-extension
plan_ref: "acp-error-notification-964"
---

## Problem

Sub-agent model errors forwarded through ACP accumulated into the parent agent's `response_text`, polluting downstream LLM input and potentially confusing later turns. The external `agent-client-protocol` `SessionUpdate` enum has no dedicated error variant, so `harnx-acp-server` previously forwarded `ModelEvent::Error` as plain `AgentMessageChunk` text with `error: {err}` prefix. Parent client accumulated all chunks into `response_text`, unaware the content was an error.

## Symptoms

- Sub-agent errors appeared in parent agent's next-turn prompt context
- TUI rendered errors correctly (without accumulation), but ACP path behaved differently
- Error text like `error: upstream failed` mixed with legitimate response content
- Downstream LLM could misinterpret error text as part of conversation

## Investigation Steps

1. Traced `ModelEvent::Error` handling in `harnx-acp-server` — mapped to `AcpForward::Text` with `error: ` prefix
2. Identified `AcpNotificationClient::session_notification` accumulated all `AgentMessageChunk` into `response_text`
3. Noted `is_agent_message` bool controlled accumulation — no differentiation for error chunks
4. Discovered existing `harnx:*` meta-marker convention (`harnx:model`, `harnx:markdown`, `harnx:usage`)
5. Recognized pattern: meta markers convey harnx-specific semantics over ACP wire without protocol fork

## Root Cause

**Protocol gap**: External `SessionUpdate` enum lacks error variant. Server code forced all forwarded content through `AgentMessageChunk`, conflating errors with genuine message text.

**Accumulation logic flaw**: Client accumulated all `AgentMessageChunk` chunks into `response_text` based on a single `is_agent_message` flag, with no condition to exclude error content.

## Solution

### 1. Server: Add `AcpForward::Error` variant and `send_notify_error`

Added internal `AcpForward::Error(String, Option<AgentSource>)` variant:

```rust
// crates/harnx-acp-server/src/lib.rs
enum AcpForward {
    Text(String, Option<AgentSource>),
    Error(String, Option<AgentSource>),  // NEW
    UserText(String, Option<AgentSource>),
    // ...
}

// Map ModelEvent::Error to AcpForward::Error
AgentEvent::Model(ModelEvent::Error(err)) if !err.is_empty() => {
    Some(AcpForward::Error(err, source))  // was: AcpForward::Text(format!("error: {err}"), source))
}
```

`send_notify_error` emits `AgentMessageChunk` with `harnx:error: true` in meta:

```rust
fn send_notify_error(
    conn: &Option<acp::ConnectionTo<acp::Client>>,
    session_key: &str,
    err: String,
    source: Option<AgentSource>,
) {
    let mut meta = source.as_ref().and_then(meta_from_source).unwrap_or_default();
    meta.insert("harnx:error".to_string(), serde_json::Value::Bool(true));
    let chunk = ContentChunk::new(format!("error: {err}").into()).meta(meta);
    let notification = SessionNotification::new(
        SessionId::new(session_key.to_string()),
        SessionUpdate::AgentMessageChunk(chunk),
    );
    let _ = conn.send_notification(notification);
}
```

### 2. Client: Check meta for error flag, exclude from accumulation

Added `is_error_from_meta_value` helper and `accumulate_response_text` flag:

```rust
// crates/harnx-acp/src/client.rs
fn is_error_from_meta_value(value: &serde_json::Value) -> bool {
    value
        .get("harnx:error")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)  // fails open: missing/non-bool = not error
}

fn strip_error_prefix(text: &str) -> &str {
    text.strip_prefix("error: ").unwrap_or(text)
}

// In session_notification:
let mut accumulate_response_text = false;
match args.update {
    SessionUpdate::AgentMessageChunk(chunk) => {
        let is_error = meta.as_ref().is_some_and(is_error_from_meta_value);
        if is_error {
            Some(AgentEvent::Model(ModelEvent::Error(
                strip_error_prefix(&text).to_string(),
            )))
        } else {
            accumulate_response_text = true;  // only set for non-error
            Some(AgentEvent::Model(ModelEvent::MessageChunk { ... }))
        }
    }
    // ...
}
```

## Why This Works

**Meta marker pattern**: Embeds harnx-specific semantics in existing ACP chunk type via namespaced `harnx:<key>` meta. External protocol remains unchanged; unaware clients still render the human-readable `error: {err}` text.

**Fail-open design**: `is_error_from_meta_value` uses strict bool check with `unwrap_or(false)`. Missing key, non-bool values, or malformed meta all fall back to normal message handling. No silent breakage.

**Backward compatibility**: Both directions preserved:
- Old server → new client: No `harnx:error` meta, falls back to normal message accumulation (old behavior)
- New server → old client: Chunk renders as text with `error: ` prefix (old behavior), no meta extraction

## Prevention Strategies

**Test coverage**:
- Error chunk surfaces as `ModelEvent::Error`, NOT accumulated into `response_text`
- `harnx:error: false` treated as normal message, DOES accumulate
- Non-bool `harnx:error` value falls back to normal message handling
- Error without `error: ` prefix still surfaces raw text

**Verification**:
```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run  # NOT cargo test
```

**Code review checklist**:
- [ ] New ACP extensions use `harnx:*` namespaced meta keys
- [ ] Meta extraction functions fail open (default to safe behavior)
- [ ] Tests cover both error and non-error cases
- [ ] Backward compatibility preserved in both directions

## Related Issues

- **GitHub:** [#964](https://github.com/dobesv/harnx/issues/964) — ACP: forward ModelEvent::Error as dedicated error notification
- **Related Solution:** [mcp-tool-template-acp-propagation-2026-04-30.md](./mcp-tool-template-acp-propagation-2026-04-30.md) — Establishes `harnx:markdown` meta marker pattern

## Out of Scope

`NoticeEvent::Error`/`Warning` still forwarded as plain text and can pollute `response_text` — same class of issue, could reuse `harnx:error` path in future.
