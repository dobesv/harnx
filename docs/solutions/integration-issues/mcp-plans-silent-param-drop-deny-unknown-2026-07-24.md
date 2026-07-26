---
title: "Silent data loss from serde dropping unknown fields at MCP tool boundaries"
date: 2026-07-24
category: integration-issues
problem_type: integration_issue
component: harnx-mcp-plans
root_cause: serde default behavior silently ignores unknown fields at RPC boundaries
resolution_type: code_fix
severity: critical
tags:
  - mcp
  - serde
  - rust
  - deny_unknown_fields
  - silent-data-loss
  - tool-params
plan_ref: plans-github-body-and-subissue-fixes
---

## Problem

Plans created via the GitHub-backed MCP server had empty bodies even though the tool call included a `content:` parameter. The `content` field did not exist in the param struct; serde silently dropped it; all body params were `None`; the plan body was written empty; the tool reported success.

## Symptoms

- Plan created with `content: "large body text"` resulted in an issue with only YAML frontmatter
- No error returned to caller — tool reported success
- Real incident: GitHub issue #1182 created via `add_plan` with body content, received empty plan

## Investigation Steps

1. Traced `add_plan` handler — discovered the param struct (`AddPlanParams`) had `replace_content`, `append_content`, `replace_in_content` fields, but NO `content` field
2. Checked serde behavior — by default, `serde_json::from_value` ignores unknown fields without error
3. Found tool docstrings advertised a `content` param that didn't exist in the struct — schema/doc mismatch
4. Identified the general hazard: all plan/task/note param structs were vulnerable to the same class of bug

## Root Cause

Serde's default deserialization behavior ignores unknown JSON fields. At MCP tool boundaries, this means:

1. Client sends `{ "name": "my-plan", "content": "body text" }`
2. `AddPlanParams` struct has no `content` field
3. Serde deserializes successfully, dropping the unknown `content` key
4. Handler receives `content: None`, all body params `None`
5. Plan created with empty body
6. Tool returns success — no indication anything was lost

This is the same bug class as typo'd field names (`contnet` vs `content`) — any unrecognized key is silently discarded.

## Solution

### 1. Added missing `content` field

Added `content: Option<String>` to `AddPlanParams` and `UpdatePlanParams` in both `harnx-mcp-plans-core` and `harnx-mcp-plans` crates:

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddPlanParams {
    pub name: String,
    pub content: Option<String>,  // NEW: maps to full-body replace
    pub replace_content: Option<String>,
    pub append_content: Option<String>,
    // ...
}
```

Handler validates mutual exclusion — at most one body param allowed:

```rust
fn add_plan_body(params: &AddPlanParams) -> Result<Option<String>, ErrorData> {
    match (params.body.as_ref(), params.content.as_ref()) {
        (Some(_), Some(_)) => Err(ErrorData::invalid_params(
            "provide at most one of body, content",
            None,
        )),
        (Some(body), None) => Ok(Some(body.clone())),
        (None, Some(content)) => Ok(Some(content.clone())),
        (None, None) => Ok(None),
    }
}
```

### 2. Added `#[serde(denie_unknown_fields)]` to all param structs

Applied to all 17 tool parameter structs across both crates:

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]  // NEW: reject unknown fields with error
pub struct AddPlanParams { ... }
```

Now, any unknown field returns a clear error:

```json
{
  "code": "INVALID_PARAMS",
  "message": "unknown field `contnet` at line 1 column 12"
}
```

### 3. Updated manual JSON schemas

The crate uses hand-written `impl_json_schema!` macro blocks (dual source of truth — a known smell). Updated schemas to include `content` and `additionalProperties: false`:

```rust
impl_json_schema!(AddPlanParams, {
    // ...
    properties.insert("content".into(), json!({
        "type": "string",
        "description": "Full plan body content, replacing entire body."
    }));
    // ...
    additional_properties: false,
});
```

Test added to verify schema/struct sync:

```rust
#[test]
fn plan_content_params_round_trip_and_appear_in_schemas() {
    let schema = AddPlanParams::json_schema();
    assert!(schema["properties"]["content"].is_object());
    assert_eq!(schema["additionalProperties"], Value::Bool(false));
}
```

## Why This Works

`#[serde(deny_unknown_fields)]` changes serde's behavior from "ignore unknown" to "error on unknown". At trust boundaries (MCP JSON-RPC surface), this converts silent data loss into explicit client errors:

- Typo'd field name → error instead of silent ignore
- Schema drift → error instead of silent partial data
- Client expecting different API version → error instead of corrupted state

This is the safer default for tool/ RPC boundaries where the caller expects their input to be fully processed.

## Prevention Strategies

**Test Cases:**

- `file_store_argument_parser_rejects_unknown_fields` — unknown field `bogus` returns error containing "unknown field"
- `add_plan_rejects_body_with_content` — conflict between `body` and `content` returns invalid params
- `plan_content_params_round_trip_and_appear_in_schemas` — verify schema sync

**Code Review Checklist:**

- [ ] All MCP tool param structs have `#[serde(deny_unknown_fields)]`
- [ ] New fields added to structs are also added to manual JSON schemas
- [ ] Parameter conflicts are explicitly validated before side effects
- [ ] Tool docstrings list real params that exist in the struct

**General Principle:**

> For any tool/RPC param boundary, silently ignoring unknown fields turns client mistakes into silent data loss with false-success responses. Prefer `deny_unknown_fields` (or explicit validation) at trust boundaries.

**Caveats:**

- This is a breaking change for clients sending extra/unknown fields — appropriate for minor version bump in pre-1.0
- Storage/frontmatter structs intentionally omit `deny_unknown_fields` to remain forward-compatible with persisted state
- Manual JSON-schema implementations must stay in sync with structs — consider auto-generation in future

## Related Issues

- **GitHub Issue:** #1182 — The incident that surfaced this bug (plan body silently dropped)
- **Related Solution:** [integration-issues/mcp-schema-ux-native-params-2026-05-11.md](mcp-schema-ux-native-params-2026-05-11.md) — Schema description accuracy for MCP tools
- **Related Solution:** [logic-errors/mcp-plans-rewrite-patterns-2026-05-04.md](../logic-errors/mcp-plans-rewrite-patterns-2026-05-04.md) — Original plans MCP server patterns
