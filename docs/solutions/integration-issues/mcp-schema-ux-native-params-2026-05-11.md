---
title: "MCP schema UX: per-field descriptions guide agents toward native params"
date: 2026-05-11
category: "integration-issues"
problem_type: integration_issue
component: "harnx-mcp-servers"
root_cause: "missing schema descriptions led agents to use shell pipes instead of native tool parameters"
resolution_type: code_fix
severity: medium
tags:
  - mcp
  - schema
  - json-schema
  - agent-behavior
  - descriptions
plan_ref: "mcp-schema-agent-ux"
---

## Problem

MCP tool schemas in `harnx-mcp-bash` and `harnx-mcp-fs` lacked per-field descriptions. Agents (Claude) bypassed native tool parameters (`head_lines`, `tail_lines`, `grep`, `offset`, `limit`) and used shell pipes (`| head`, `| tail`, `| grep`) instead, reducing efficiency and losing structured output handling.

## Symptoms

- Agents wrote `command: "git log --oneline | head -20"` instead of using `head_lines: 20`
- Shell pipelines made output post-processing harder (no incremental streaming, no structured metadata)
- No guidance in schema for agents to prefer native params
- Field semantics unclear (line vs byte, optional vs required)

## Investigation Steps

1. Reviewed `list_tools` output — `inputSchema` had `properties` but no `description` fields
2. Compared `harnx-mcp-plans` (which had a richer schema helper) to `harnx-mcp-bash`/`harnx-mcp-fs`
3. Identified the pattern: `harnx-mcp-plans` used `object_schema_with_desc(vec![(name, desc, schema), ...], required)`
4. Decided to centralize this helper in `harnx-mcp` shared crate and migrate all servers

## Root Cause

Schema generation used `object_schema()` helper producing bare JSON schema objects without descriptions. When agents inspect `inputSchema`, they see field names only — no guidance on semantics or preferred usage patterns.

The fix required:
1. Centralizing `object_schema_with_desc()` in `harnx-mcp/src/schema.rs`
2. Migrating all `JsonSchema` impls in bash/fs servers to use it
3. Writing accurate descriptions that mention native params and discourage shell anti-patterns

## Solution

### 1. Shared helper in `harnx-mcp/src/schema.rs`

```rust
/// Build a JSON Schema object where each property carries a `description`.
pub fn object_schema_with_desc(
    properties: Vec<(&str, &str, Schema)>,
    required: &[&str],
) -> Schema {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));

    let mut property_map = Map::new();
    for (name, desc, property_schema) in properties {
        let mut prop = property_schema.as_value().clone();
        if let Some(obj) = prop.as_object_mut() {
            obj.insert("description".to_string(), Value::String(desc.to_string()));
        }
        property_map.insert(name.to_string(), prop);
    }
    schema.insert("properties".to_string(), Value::Object(property_map));
    schema.insert("additionalProperties".to_string(), Value::Bool(false));

    if !required.is_empty() {
        schema.insert(
            "required".to_string(),
            Value::Array(
                required.iter().map(|n| Value::String((*n).to_string())).collect(),
            ),
        );
    }
    schema.into()
}
```

### 2. Migration pattern

**Before (no descriptions):**
```rust
object_schema(
    vec![
        ("command", command),
        ("head_lines", head_lines),
    ],
    &["command"],
)
```

**After (with descriptions):**
```rust
object_schema_with_desc(
    vec![
        ("command", "Bash command to execute. Avoid shell pipes like | head, | tail, | grep — use head_lines, tail_lines, max_output_bytes instead.", command),
        ("head_lines", "Return only the first N lines of combined output. Prefer this over `| head -N` in the command.", head_lines),
    ],
    &["command"],
)
```

### 3. Accuracy rules discovered during review

**Critical:** Schema descriptions must exactly match implementation:
- If a tool has no `grep` param, do NOT mention `grep` in its descriptions
- If `offset`/`limit`/`tail` are line-based, descriptions must say "lines" not "bytes"
- Use same terminology across all tools for consistency

### 4. Accepted duplication

`harnx-mcp-plans` still has a local copy of `object_schema_with_desc` — not migrated because it would require adding a new dependency. This is accepted technical debt.

## Why This Works

**Agent behavior guidance:** MCP agents read `inputSchema` descriptions to understand tool parameters. Adding explicit "Prefer X over Y" guidance directly shapes agent behavior.

**Semantics clarity:** Describing `head_lines` as "Return only the first N lines" removes ambiguity about line vs byte semantics.

**Discoverability:** Field descriptions appear in `list_tools` output, making native params discoverable without reading source code.

## Prevention Strategies

**Schema description checklist:**
- [ ] Every field has a description
- [ ] Descriptions match actual semantics (lines vs bytes, 0-indexed vs 1-indexed)
- [ ] Tool-level descriptions mention native params, not shell alternatives
- [ ] If a param doesn't exist, don't mention it in descriptions
- [ ] Prefer consistent phrasing across all servers

**Review verification:**
- Inspect `inputSchema` via `list_tools` after changes
- Verify no shell-pipe guidance references unsupported params
- Check line-vs-byte wording matches implementation

**Test pattern:**
- Add unit tests for `object_schema_with_desc` in shared crate
- Add snapshot tests that verify `inputSchema` includes descriptions

## Related Issues

- **GitHub:** #491 — MCP schema UX improvements
- **GitHub:** #513 — Agents using shell pipes instead of native params
- **Changeset:** `.changesets/mcp-schema-agent-ux.md`
- **Related Solution:** [integration-issues/mcp-tool-template-design-guidelines-2026-05-08.md](./mcp-tool-template-design-guidelines-2026-05-08.md) — Tool call_template design (display-side)
