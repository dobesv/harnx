---
title: "ACP session_prompt blank session_id handling and actionable error messages"
date: 2026-07-29
category: integration-issues
problem_type: integration_issue
component: harnx-acp
root_cause: "blank strings passed verbatim to server; unactionable JSON-RPC error messages"
resolution_type: code_fix
severity: medium
tags:
  - acp
  - sub-agent
  - json-rpc
  - error-messages
  - llm-tool-design
  - session-management
plan_ref: issue-1282-acp-session-validation
---

## Problem

ACP `session_prompt` tool failed with a bare "Invalid params" error when sub-agent models passed `session_id: ""` or invented session IDs. The error lacked context for the model to self-correct, causing repeated failed attempts.

## Symptoms

- `session_id: ""` forwarded verbatim to server instead of triggering session auto-creation
- Unknown session IDs returned JSON-RPC error code `-32602` with empty message body
- Models couldn't determine correct behavior from error — no remediation guidance
- Delegation workflows blocked by uninformative failures

## Investigation Steps

1. Traced `session_prompt` handler in `harnx-acp/src/manager.rs` — `optional_string` helper returned `Some("")` for empty strings, passing them to server
2. Checked `get_or_build_session` in `harnx-acp-server/src/lib.rs` — returned `acp::Error::invalid_params()` with no message on unknown session
3. Confirmed the pattern: both empty strings and sentinel values like `"new"` were treated as lookup keys rather than "omit" signals
4. Identified same bare error construction in `cancel` path (memory-only lookup, no disk check)

## Root Cause

Two related issues:

1. **Blank-as-content problem**: `optional_string` treated empty strings as meaningful values rather than absent parameters. Models frequently send `session_id: ""` as a "no value" sentinel, but the code interpreted it as a lookup key.

2. **Message-free error problem**: `acp::Error::invalid_params()` returned only the JSON-RPC error code with no explanatory message. The calling model received no context on what went wrong or how to fix it.

Both issues stem from a general principle: LLM-facing tool boundaries need different error-handling semantics than human-facing APIs. Models cannot debug interactively; they need self-contained, actionable error messages.

## Solution

### 1. Blank string normalization

Renamed `optional_string` to `optional_nonblank_string` and added whitespace check:

```rust
fn optional_nonblank_string<'a>(
    arguments: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>> {
    match arguments.get(key) {
        Some(Value::String(value)) if value.trim().is_empty() => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(anyhow!("ACP tool argument '{}' must be a string", key)),
    }
}
```

Empty and whitespace-only strings now map to `None`, triggering `session_new()` auto-creation per the original intent.

### 2. Actionable error message

Added `unknown_session_error` helper in `harnx-acp-server/src/lib.rs`:

```rust
/// Build the actionable ACP error returned when a caller references a session
/// ID that isn't in memory or on disk. The message tells the calling model how
/// to recover (use a real ID or omit it) instead of a bare "Invalid params".
fn unknown_session_error(session_id: &str) -> acp::Error {
    acp::Error::new(
        -32602,
        format!(
            "Unknown session ID '{session_id}'. Use a session_id returned by session_new or session_prompt; omit it to start a new session."
        ),
    )
}
```

Applied in both `cancel` (line 740) and `get_or_build_session` lazy-load (line 764) paths.

### 3. Explicit tool descriptions

Extracted guidance to `SESSION_ID_GUIDANCE` constant to prevent drift:

```rust
const SESSION_ID_GUIDANCE: &str = "To continue a conversation, pass only the exact session_id returned by session_prompt or session_new. To start a new conversation, omit session_id; empty or whitespace-only values also start a new session. Do not invent a session ID.";
```

Included in both parameter description and tool description.

### 4. Test coverage

- `test_cancel_unknown_session_errors` — verifies error code `-32602` and exact message for cancel path
- `test_prompt_unknown_session_errors` — verifies same error for prompt/lazy-load path
- `optional_nonblank_string_treats_blank_values_as_omitted` — covers empty, whitespace, null, absent, wrong-type cases

## Why This Works

**Blank normalization**: Models often use `""` as a "no value" signal when they don't have an ID to provide. Treating blank/whitespace as absent matches how models actually behave, avoiding a common failure mode without requiring models to explicitly omit the parameter.

**Actionable messages**: JSON-RPC error code `-32602` alone provides nothing the model can act on. Including specific instructions ("Use a session_id returned by...; omit it to start a new session") gives the model a clear recovery path in the next turn. This is essential when the caller cannot interactively debug.

**Constant guidance**: Extracting `SESSION_ID_GUIDANCE` prevents drift between parameter and tool descriptions. When guidance must stay synchronized across multiple strings, a single source of truth eliminates maintenance gaps.

## Prevention Strategies

**When designing LLM-facing tool parameters:**

- Treat blank/whitespace strings as absent for optional ID parameters — models use `""` as a natural "no value" signal
- Return actionable error messages with specific remediation steps, not just error codes
- Synchronize guidance text via constants to prevent description drift

**Code review checklist:**

- [ ] Do optional ID params treat `""` as `None`?
- [ ] Does every JSON-RPC error include a message explaining how to recover?
- [ ] Are guidance strings consolidated to prevent drift?

**Test coverage:**

- Assert exact error code AND message text for RPC errors
- Test blank string, whitespace, `null`, and absent key for normalization functions
- Cover all call sites that construct errors (not just one)

## Related Issues

- **GitHub:** [#1282](https://github.com/dobesv/harnx/issues/1282) — ACP session_prompt empty session_id failure
- **Related Solution:** [integration-issues/mcp-plans-silent-param-drop-deny-unknown-2026-07-24.md](./mcp-plans-silent-param-drop-deny-unknown-2026-07-24.md) — Parameter handling at tool boundaries
- **Related Solution:** [logic-errors/xdg-directory-separation-2026-05-03.md](../logic-errors/xdg-directory-separation-2026-05-03.md) — Empty-string guards for env vars
