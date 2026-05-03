---
title: "Plan note file storage with per-note files and JSON overview for read_plan"
date: 2026-05-03
category: integration-issues
problem_type: integration_issue
component: harnx-mcp-todo
root_cause: plan_add_note appended to plan.md instead of writing dedicated note files; read_plan returned raw text instead of structured JSON
resolution_type: code_fix
severity: medium
tags:
  - mcp-tools
  - plan-notes
  - json-response
  - file-per-note
plan_ref: fix-plan-add-note
---

## Problem

`plan_add_note` incorrectly appended notes to `plan.md` instead of writing dedicated `note-<id>.md` files. `read_plan` returned raw `plan.md` text, forcing consumers to parse it themselves. No tool existed to read a specific note by ID.

## Symptoms

- Calling `plan_add_note` multiple times modified `plan.md` with appended `### Note` sections
- No `note-*.md` files created in plan directory
- No way to enumerate or read individual notes
- `read_plan` consumers had to parse raw markdown text

## Investigation Steps

Reviewed `handle_plan_add_note` in `server.rs` — confirmed it appended to `plan.md`. Traced PR #424 which established `todo-<id>.md` pattern for todos. Issue #392 specified same pattern for notes but wasn't implemented. Executed `cargo test -p harnx-mcp-todo` — existing test `plan_add_note_creates_missing_plan_file` failed on expected summary format, confirming old behavior.

## Root Cause

Design oversight: `plan_add_note` predated the per-file pattern. `read_plan` returned raw text because no JSON structure was specified. The note functionality needed:

1. `note_file_path()` helper (parallel to `todo_file_path()`)
2. `handle_plan_add_note` to write `note-<id>.md` files
3. `handle_plan_read` to scan directory and return JSON overview
4. New `plan_read_note` tool to read note content by ID

## Solution

### 1. Note File Path Helper

```rust
fn note_file_path(dir: &Path, plan_name: &str, id: &str) -> PathBuf {
    plan_dir(dir, plan_name).join(format!("note-{}.md", id))
}
```

### 2. Fixed `handle_plan_add_note`

```rust
fn handle_plan_add_note(&self, params: PlanAddNoteParams) -> Result<CallToolResult, ErrorData> {
    let name = normalize_plan_name(&params.name)
        .map_err(|err| ErrorData::invalid_params(err, None))?;
    let id = generate_id();
    let dir = plan_dir(&self.dir, &name);
    std::fs::create_dir_all(&dir).map_err(|err| {
        ErrorData::internal_error(format!("failed to create {}: {err}", dir.display()), None)
    })?;
    let path = note_file_path(&self.dir, &name, &id);
    std::fs::write(&path, &params.text).map_err(|err| {
        ErrorData::internal_error(format!("failed to write {}: {err}", path.display()), None)
    })?;
    Ok(result_text(
        format!("Created note {id} in plan {name}"),
        format!("Created note {id} in plan {name}"),
    ))
}
```

### 3. Enhanced `handle_plan_read` with JSON Response

```rust
fn handle_plan_read(&self, params: PlanReadParams) -> Result<CallToolResult, ErrorData> {
    let name = normalize_plan_name(&params.name)
        .map_err(|err| ErrorData::invalid_params(err, None))?;
    let path = plan_file_path(&self.dir, &name);
    let dir = plan_dir(&self.dir, &name);
    if !dir.exists() {
        return Err(ErrorData::invalid_params(
            format!("plan '{name}' not found"),
            None,
        ));
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(ErrorData::internal_error(
                format!("failed to read {}: {err}", path.display()),
                None,
            ))
        }
    };
    let mut note_ids = Vec::new();
    let mut todo_ids = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if let Some(note_id) = file_name
                .strip_prefix("note-")
                .and_then(|name| name.strip_suffix(".md"))
            {
                note_ids.push(note_id.to_string());
            } else if let Some(todo_id) = file_name
                .strip_prefix("todo-")
                .and_then(|name| name.strip_suffix(".md"))
            {
                todo_ids.push(todo_id.to_string());
            }
        }
    }

    note_ids.sort();
    todo_ids.sort();

    Ok(result_json(
        json!({
            "plan": name,
            "content": content,
            "note_ids": note_ids,
            "todo_ids": todo_ids,
        }),
        format!("Read plan {name}"),
    ))
}
```

### 4. New `plan_read_note` Tool

```rust
struct PlanReadNoteParams {
    plan: String,
    note_id: String,
}

fn handle_plan_read_note(&self, params: PlanReadNoteParams) -> Result<CallToolResult, ErrorData> {
    let plan = normalize_plan_name(&params.plan)
        .map_err(|err| ErrorData::invalid_params(err, None))?;
    let raw_id = params.note_id.trim();
    let id = raw_id
        .strip_prefix("note-")
        .unwrap_or(raw_id)
        .to_ascii_lowercase();
    let path = note_file_path(&self.dir, &plan, &id);
    let content = std::fs::read_to_string(&path).map_err(|_| {
        ErrorData::invalid_params(format!("note '{id}' not found in plan '{plan}'"), None)
    })?;
    Ok(result_text(content, format!("Read note {id} in plan {plan}")))
}
```

## Why This Works

**Per-file notes**: Each note gets `note-<id>.md` — independent lifecycle, no merge conflicts, easy enumeration via directory scan.

**JSON overview**: `{plan, content, note_ids, todo_ids}` gives consumers structured data without parsing markdown. Bare IDs (`abcd1234`) without prefix — cleaner API.

**Note-only plans**: `handle_plan_read` checks directory existence before `plan.md`. Plan dir exists without `plan.md` = valid (returns `content: ""`). Enables note-first workflows.

**Defensive normalization**: `handle_plan_read_note` trims whitespace, strips `note-` prefix if caller provides it, lowercases for case-insensitive matching. Order matters: strip before lowercase to handle mixed-case prefixes.

## Prevention Strategies

**Test Cases:**
- `plan_add_note` creates `note-<id>.md`, never touches `plan.md`
- `plan_add_note` multiple times creates separate files
- `read_plan` returns JSON with correct `note_ids` and `todo_ids`
- `read_plan` handles missing `plan.md` (returns empty content)
- `read_plan` errors when plan directory missing
- `plan_read_note` normalizes `note-` prefix, whitespace, case

**Code Review Checklist:**
- [ ] Does `handle_plan_read` check directory before `plan.md`?
- [ ] Does note ID normalization handle prefix case correctly?
- [ ] Are note files plain markdown (no YAML frontmatter)?

## Related Issues

- **Plan:** fix-plan-add-note
- **Issue:** [#392](https://github.com/dobesv/harnx/issues/392)
- **Prior Solution:** [logic-errors/mcp-todo-fs-restructure-2026-05-01.md](../logic-errors/mcp-todo-fs-restructure-2026-05-01.md) — established `todo-<id>.md` pattern and YAML frontmatter
