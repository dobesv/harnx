---
title: "Surgical edit API pattern for MCP plan mutations"
date: 2026-05-11
category: "api-design"
problem_type: logic_error
component: "harnx-mcp-plans"
root_cause: "full-body replacement API unsuitable for incremental agent edits"
resolution_type: code_fix
severity: medium
tags:
  - mcp
  - api-design
  - surgical-edits
  - unified-diffs
  - rust
plan_ref: "harnx-512-improve-plan-editing"
---

## Problem

`update_plan` and `update_task` used full-body replacement fields (`content`, `body`). Agents making incremental edits had to read the full body, modify it, and write it back — error-prone and wasteful. No diff output made it hard for agents to verify changes.

## Symptoms

- Agents struggled to make small edits without clobbering existing content
- No visibility into what changed after an update operation
- `append_task` tool existed but only for tasks, not plans or notes
- Full-body reads required before any edit operation

## Investigation Steps

Reviewed `update_plan` and `update_task` handlers. Both accepted optional `content`/`body` fields that replaced the entire body. Agents calling these tools had no way to append text or perform targeted string replacement without reading the entire body first.

Identified three common edit patterns from agent usage:
1. Replace entire body (rare, but needed)
2. Append content to existing body
3. Find-and-replace a specific substring

Designed mutually exclusive fields to cover all three patterns.

## Root Cause

API design forced agents into read-modify-write pattern for simple edits. No atomic append or replace-in-place operations. No feedback mechanism (diffs) to verify changes applied correctly.

## Solution

### 1. Surgical Edit Fields

Replaced `content`/`body` fields with three mutually exclusive alternatives in `UpdatePlanParams`, `UpdateTaskParams`, and new `UpdateNoteParams`:

```rust
#[derive(Debug, Default, Clone, Deserialize)]
struct UpdatePlanParams {
    // ... other fields ...
    #[serde(default)]
    replace_content: Option<String>,
    #[serde(default)]
    append_content: Option<String>,
    #[serde(default)]
    replace_in_content: Option<ReplaceInContent>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct ReplaceInContent {
    old_text: String,
    new_text: String,
    #[serde(default)]
    replace_all: Option<bool>,
}
```

### 2. Mutual Exclusion Enforcement

```rust
let body_count = [
    params.replace_content.is_some(),
    params.append_content.is_some(),
    params.replace_in_content.is_some(),
]
.iter()
.filter(|&&b| b)
.count();
if body_count > 1 {
    return Err(ErrorData::invalid_params(
        "at most one of replace_content, append_content, replace_in_content may be provided",
        None,
    ));
}
```

### 3. Append Newline Logic

When appending, ensure newline separator exists between existing and new content:

```rust
if let Some(ac) = params.append_content {
    let mut b = existing_body;
    if !b.is_empty() && !b.ends_with('\n') {
        b.push('\n');
    }
    b.push_str(&ac);
    b
}
```

### 4. Empty `old_text` Guard

`apply_replace_in` rejects empty `old_text` to prevent Rust's `replace("", ...)` from inserting at every character boundary:

```rust
fn apply_replace_in(body: &str, r: &ReplaceInContent) -> Result<String, ErrorData> {
    if r.old_text.is_empty() {
        return Err(ErrorData::invalid_params(
            "old_text must not be empty",
            None,
        ));
    }
    if !body.contains(&*r.old_text) {
        return Err(ErrorData::invalid_params(
            format!("old_text {:?} not found in body", r.old_text),
            None,
        ));
    }
    let result = if r.replace_all == Some(true) {
        body.replace(&*r.old_text, &r.new_text)
    } else {
        body.replacen(&*r.old_text, &r.new_text, 1)
    };
    Ok(result)
}
```

### 5. diff_text Helper

Unified diff output using `similar` crate:

```rust
fn diff_text(before: &str, after: &str, path: &str) -> String {
    if before == after {
        return String::new();
    }
    let diff = TextDiff::from_lines(before, after);
    let mut output = format!("--- a/{path}\n+++ b/{path}\n");
    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            let sign = match change.tag() {
                ChangeTag::Delete => "-",
                ChangeTag::Insert => "+",
                ChangeTag::Equal => " ",
            };
            let value = change.value();
            output.push_str(sign);
            output.push_str(value);
            if !value.ends_with('\n') {
                output.push('\n');
            }
        }
    }
    format!("```diff\n{output}```")
}
```

Diff only covers body/text content, not YAML frontmatter, to reduce noise.

### 6. delete_plan Diff Collection

Read all plan files before deletion to emit per-file diffs:

```rust
let plan_file = plan_file_path(&self.dir, &name);
if plan_file.exists() {
    let content = std::fs::read_to_string(&plan_file).unwrap_or_default();
    let d = diff_text(&content, "", &format!("{name}/plan.md"));
    if !d.is_empty() {
        diffs.push(d);
    }
}

// Similar for tasks/*.md and notes/*.md
for f in files {
    let content = std::fs::read_to_string(&f).unwrap_or_default();
    let d = diff_text(&content, "", &format!("{name}/tasks/{stem}.md"));
    if !d.is_empty() {
        diffs.push(d);
    }
}
```

### 7. impl_json_schema! Macro

Consistent schema generation for all param types:

```rust
macro_rules! impl_json_schema {
    ($type:ty, $title:expr, $properties_fn:expr, $required:expr) => {
        impl JsonSchema for $type {
            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed($title)
            }
            fn schema_id() -> Cow<'static, str> {
                Cow::Borrowed(concat!(module_path!(), "::", $title))
            }
            fn json_schema(gen: &mut SchemaGenerator) -> Schema {
                object_schema_with_desc($properties_fn(gen), $required)
            }
        }
    };
}

impl_json_schema!(
    ReplaceInContent,
    "ReplaceInContent",
    |gen: &mut SchemaGenerator| vec![
        ("old_text", "Text to find and replace", gen.subschema_for::<String>()),
        ("new_text", "Replacement text", gen.subschema_for::<String>()),
        ("replace_all", "Replace all occurrences if true", gen.subschema_for::<bool>()),
    ],
    &["old_text", "new_text"]
);
```

## Why This Works

- **Atomic operations**: Single tool call performs edit without prior read
- **Mutual exclusion**: Clear API contract prevents conflicting edit modes
- **Diff feedback**: Agents see exactly what changed
- **Guard rails**: Empty `old_text` rejection prevents edge-case bugs
- **Consistent schema**: Macro ensures all param types have proper MCP tool schemas

## Prevention Strategies

- Document the three edit modes in tool descriptions
- Add tests for mutual exclusion enforcement
- Add tests for empty `old_text` rejection
- Add tests for append newline logic with non-empty and empty bodies
- Include diff output in all mutation tool responses

## Related Issues

- GitHub: [#512](https://github.com/dobesv/harnx/issues/512)
- Commit: `71c4a528` — feat(mcp-plans): surgical body edits, diff output, update_note, remove append_task
- Commit: `0a73e40c` — fix(mcp-plans): reject empty old_text in replace_in_* handlers
