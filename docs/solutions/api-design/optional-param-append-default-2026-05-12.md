---
title: "Making MCP tool parameters optional with append-to-end default"
date: 2026-05-12
category: "api-design"
problem_type: integration_issue
component: "harnx-mcp-fs"
root_cause: "required usize parameter prevented natural append-to-end usage pattern"
resolution_type: code_fix
severity: medium
tags:
  - mcp
  - api-design
  - serde
  - optional-parameters
  - defaults
plan_ref: "mcp-fs-insert-append"
---

## Problem

`fs_insert` tool required `insert_line: usize` parameter. Users had to determine file line count before appending, forcing read-then-insert workflows. The natural append-to-end pattern was unnecessarily verbose.

## Symptoms

- Callers had to read file first to get `total_lines` before appending
- Parameter documentation mentioned `insert_line: N` where "N = total lines appends at end" but required explicit value
- No natural default for append operation

## Investigation Steps

Reviewed `InsertParams` struct and identified `insert_line` as required `usize`. Evaluated two approaches:
1. Keep `usize` but use sentinel value (e.g., `usize::MAX`) for append
2. Change to `Option<usize>` with `None` meaning append

Chose option 2 — serde `None` default semantically clearer than sentinel value. Checked how this pattern propagates through JsonSchema impl and tool template rendering.

## Root Cause

Serde deserialization and schemars-rs `JsonSchema` both require coordination to make a parameter truly optional with a meaningful default. Changing struct field type alone is insufficient — schema generation must also remove field from `required` array.

## Solution

Three-part change to make `insert_line` optional with `None` → append behavior:

**1. Struct field with serde default:**

```rust
#[derive(Debug, Deserialize)]
pub struct InsertParams {
    pub path: String,
    #[serde(default)]
    pub insert_line: Option<usize>,  // was: usize (required)
    pub insert_text: String,
    #[serde(default)]
    pub column: Option<usize>,
}
```

**2. Manual JsonSchema impl — remove from required array:**

```rust
impl JsonSchema for InsertParams {
    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let insert_line = generator.subschema_for::<Option<usize>>();  // was: usize
        // ...
        object_schema_with_desc(
            vec![
                ("path", "Absolute path to the file to insert into.", path),
                ("insert_line", "Insert after this line number. 0 = prepend before line 1; N = insert after line N; omit (or use N = total lines) to append to the end of the file.", insert_line),
                // ...
            ],
            &["path", "insert_text"],  // removed "insert_line" from required
        )
    }
}
```

**3. Implementation resolve default:**

```rust
async fn insert_impl(&self, params: InsertParams) -> Result<CallToolResult, ErrorData> {
    // ...
    let lines = content.split_inclusive('\n').collect::<Vec<_>>();
    let total_lines = lines.len();
    // None means append (equivalent to insert after last line)
    let insert_line = params.insert_line.unwrap_or(total_lines);
    // ...
}
```

**4. Tool template display:**

Minijinja template uses default filter for display:

```jinja2
{{ args.insert_line | default(value="end") }}
```

Shows "end" when `insert_line` absent, making tool description readable.

## Why This Works

- `#[serde(default)]` allows JSON without `insert_line` to deserialize with `None`
- `Option<usize>` in schema generates `oneOf: [null, integer]` union type
- Removing from `required` array lets callers omit field entirely
- `unwrap_or(total_lines)` computes default at runtime when file content available
- Template `default(value="end")` gives human-readable display without semantic ambiguity

## Prevention Strategies

**Pattern Checklist:**
- [ ] Add `#[serde(default)]` attribute to optional field
- [ ] Change field type from `T` to `Option<T>`
- [ ] Update `JsonSchema` impl: `subschema_for::<Option<T>>()`
- [ ] Remove field from `required` array in schema
- [ ] Resolve default with `unwrap_or(computed_default)` at point where context available
- [ ] Update tool description to document omit behavior
- [ ] Update template rendering for missing field display
- [ ] Update all test struct literals: `field: N` → `field: Some(N)`

**Best Practices:**
- Prefer `Option<T>` + `#[serde(default)]` over sentinel values
- Compute defaults at point where required context available (e.g., after reading file)
- Document omit behavior in schema description, not just docs

## Related Issues

- **Plan:** [mcp-fs-insert-append](/plans/mcp-fs-insert-append)
- **Related Solution:** [mcp-fs-insert-rereplace-tools-2026-05-11.md](../integration-issues/mcp-fs-insert-rereplace-tools-2026-05-11.md) — original insert tool design
