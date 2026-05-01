---
title: "MCP todo filesystem restructure to per-plan directories with YAML frontmatter"
date: 2026-05-01
category: logic-errors
problem_type: logic_error
component: harnx-mcp-todo
root_cause: flat file layout with JSON frontmatter, missing plan association, status filter ambiguity
resolution_type: code_fix
severity: medium
tags:
  - filesystem-layout
  - yaml-frontmatter
  - serde-yaml
  - upsert-semantics
  - status-filtering
plan_ref: gh-392-todo-fs-restructure
---

## Problem

`harnx-mcp-todo` used a flat file layout (`<data_dir>/<id>.md`) with JSON frontmatter and optional plan association. This caused issues:
1. YAML frontmatter delimiters not handled correctly with `serde_yaml` v0.9+
2. Status filter `closed`/`done` were treated separately instead of as synonyms
3. `write_plan` with keyed todos could create duplicates
4. Field preservation during upsert reset optional fields to defaults
5. Old flat files coexisted with new per-plan directories causing lookup confusion

## Symptoms

- `serde_yaml::to_string` output didn't start with `---`, causing malformed frontmatter
- `todo_list filter="closed"` didn't return todos with status `"done"` (and vice versa)
- Multiple `write_plan` calls with same todo key created duplicate entries, breaking `plan_get_todo`
- Updating a todo via upsert with `status=None` reset status to `"open"` unexpectedly
- CRLF line endings from Windows caused YAML parsing failures

## Investigation Steps

1. Traced `serialize_todo` to find YAML output missing `---` prefix — `serde_yaml` v0.9+ doesn't prepend document marker
2. Inspected `handle_list` filter logic — `filter="closed"` passed literal string to low-level filter instead of using semantic `is_closed()` check
3. Traced `handle_plan_write` — keyed todo lookup found existing but then created new instead of updating in place
4. Identified field reset in upsert path — `TodoSpec` defaults (`status=None`, `tags=[]`) overwrote existing values unconditionally
5. Checked parsing code — no CRLF normalization before YAML split on `\n---\n`

## Root Cause

**YAML frontmatter:** `serde_yaml::to_string` in v0.9+ outputs YAML without `---` prefix. The code assumed `serde_yaml` would include it, resulting in `yaml_string---\nbody` instead of `---\nyaml_string---\nbody`.

**Status filtering:** The `status_filter` logic in `list_todos` passed `filter="closed"` as a literal status string match, but the status field could be `"closed"` or `"done"`. The semantic bucket filter (`is_closed()`) wasn't applied.

**Upsert semantics:** `handle_plan_write` looked up todos by key but created new records even when existing were found, because the "else create" path was taken unconditionally when `status/body/tags/dependencies` had default values.

**Field preservation:** The upsert code used `if !tags.is_empty()` for tags and `if !dependencies.is_empty()` for dependencies, but unconditionally set `status` from `TodoSpec.status` even when `None`.

**CRLF handling:** Frontmatter parsing split on `\n---\n` without first normalizing `\r\n` and `\r` to `\n`, breaking on Windows line endings.

## Solution

### 1. YAML Frontmatter Serialization

**Before:**
```rust
fn serialize_todo(todo: &TodoRecord) -> Result<String, String> {
    let yaml = serde_yaml::to_string(&todo.front)?;
    Ok(format!("{}---\n{}", yaml, todo.body))  // Wrong: missing leading ---
}
```

**After:**
```rust
fn serialize_todo(todo: &TodoRecord) -> Result<String, String> {
    let yaml = serde_yaml::to_string(&todo.front)?;
    Ok(format!("---\n{}---\n{}", yaml, todo.body))  // Correct: prepend ---
}
```

### 2. Status Bucket Filtering

**Before:**
```rust
// handle_list passed filter directly to list_todos as status_filter
let filtered = list_todos(&self.dir, params.plan.as_deref(), params.tag.as_deref(), Some(&params.filter));
```

**After:**
```rust
fn handle_list(&self, params: TodoListParams) -> Result<CallToolResult, ErrorData> {
    let status_filter = match params.filter.as_str() {
        "all" | "open" | "closed" | "done" => None,  // Bucket filters handled separately
        other => Some(other),  // Exact status match
    };

    let mut filtered = list_todos(&self.dir, params.plan.as_deref(), params.tag.as_deref(), status_filter);

    if params.filter == "open" {
        filtered.retain(|todo| !is_closed(&todo.front.status));
    } else if matches!(params.filter.as_str(), "closed" | "done") {
        filtered.retain(|todo| is_closed(&todo.front.status));  // Either "closed" or "done"
    }
    // ...
}

fn is_closed(status: &str) -> bool {
    matches!(status.to_ascii_lowercase().as_str(), "closed" | "done")
}
```

### 3. CRLF Normalization

```rust
fn parse_yaml_frontmatter(content: &str) -> Result<(TodoFrontMatter, String), String> {
    let normalized;
    let content = if content.contains('\r') {
        normalized = content.replace("\r\n", "\n").replace('\r', "\n");
        normalized.as_str()
    } else {
        content
    };

    if !content.starts_with("---\n") {
        return Err("missing YAML front matter".to_string());
    }
    // ... rest of parsing
}
```

### 4. Upsert with Field Preservation

**In `handle_plan_write`:**
```rust
let todo = if let Some(key) = key {
    if let Some(mut existing) = find_todo_by_key(&self.dir, &name, &key) {
        // Update existing - preserve fields unless explicitly provided
        existing.front.title = title;  // Title always updated (required)
        if !tags.is_empty() {
            existing.front.tags = tags;  // Preserve if empty input
        }
        existing.front.plan = name.clone();
        if let Some(status) = status {
            existing.front.status = status;  // Preserve if None
        }
        existing.front.updated_at = Some(now_iso());
        existing.front.key = Some(key);
        if !dependencies.is_empty() {
            existing.front.dependencies = dependencies;  // Preserve if empty
        }
        if let Some(body) = body {
            existing.body = body;  // Preserve if None
        }
        existing
    } else {
        // Create new
        created += 1;
        create_todo_from_spec(...)
    }
} else {
    // No key - always create
    created += 1;
    create_todo_from_spec(...)
};
```

### 5. Directory Layout Migration

**Old layout (ignored, not migrated):**
```
.agent/todos/
  abc123.md     # Flat file with JSON frontmatter
  def456.md
```

**New layout:**
```
.agent/todos/
  plan-a/
    plan.md           # Plan content
    todo-abc123.md    # Todo with YAML frontmatter
    todo-def456.md
  plan-b/
    plan.md
    todo-xyz789.md
```

The `list_todos` function iterates subdirectories and only reads `todo-*.md` files, ignoring old flat files at root level.

## Why This Works

1. **YAML frontmatter:** Explicit `---\n` prefix matches YAML document marker convention. `serde_yaml` outputs content without it, so prepending ensures valid frontmatter.

2. **Status filtering:** Treating `"closed"` and `"done"` as synonyms via `is_closed()` matches user mental model — both represent completed work.

3. **Upsert semantics:** Checking for existing todo by key first, then updating in place, prevents duplicates. Preserving fields when input has defaults (`None`, `[]`) maintains existing state.

4. **CRLF normalization:** Converting all line endings to `\n` before parsing ensures consistent delimiter matching regardless of source platform.

5. **No migration needed:** Old files at root are simply invisible to new iteration logic, allowing gradual transition without data movement.

## Prevention Strategies

**Test Cases:**
- Verify YAML frontmatter round-trip (parse after write)
- Test `filter="closed"` returns both `"closed"` and `"done"` status todos
- Test `filter="done"` returns both `"closed"` and `"done"` status todos
- Test `write_plan` upsert: call twice with same key, verify single todo
- Test field preservation: upsert with `status=None`, verify original status kept
- Test CRLF content: parse file with `\r\n` line endings

**Code Review Checklist:**
- [ ] Does `serde_yaml` output need `---` prepended?
- [ ] Are status bucket filters using semantic checks, not literal string matches?
- [ ] Does upsert preserve existing fields when input has defaults?
- [ ] Is file content normalized before parsing?

## Related Issues

- **Plan:** gh-392-todo-fs-restructure
- **Tests:** `handle_list_closed_bucket_includes_done_and_closed`, `handle_plan_write_upserts_keyed_todos`
