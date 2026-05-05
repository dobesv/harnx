---
title: "MCP plans server rewrite patterns: file layout, atomic writes, write ordering, ID handling"
date: 2026-05-04
category: logic-errors
problem_type: logic_error
component: harnx-mcp-plans
root_cause: inconsistent file layout, missing atomic writes, improper write ordering in batch operations
resolution_type: code_fix
severity: medium
tags:
  - mcp-tools
  - file-layout
  - atomic-writes
  - write-ordering
  - id-normalization
  - legacy-parsing
plan_ref: harnx-mcp-plans-revamp
---

## Problem

`harnx-mcp-plans` required a full rewrite from `harnx-mcp-todo` with 15 tools for managing plans, tasks, and notes. Key design decisions needed to ensure consistency, atomicity, and robustness across file operations, ID handling, and batch creation.

## Symptoms

- Prior `harnx-mcp-todo` used `todo-<id>.md` prefix in filenames, creating visual noise in directory listings
- No atomic write pattern — partial writes could corrupt files on crash
- `update_plan` batch task creation could write `plan.md` with half-validated task state if key uniqueness check failed mid-way
- Legacy plans without YAML frontmatter (`---` block) would fail to parse
- ID inputs with prefixes like `task-` or `NOTE-` required manual normalization by callers

## Investigation Steps

Reviewed commits from the rewrite branch:

1. `f6b0e2d7` — initial crate with 15 tools, flat task files in `<plan>/` directory
2. `30361039` — moved tasks to `<plan>/tasks/<id>.md` subdirectory (disambiguates entity type)
3. `67ced5c2` — added atomic writes for `plan.md` files
4. `e5321d8d` — removed display prefixes, enforced key uniqueness in `update_plan`
5. `0b6677c8` — pre-built task records before any writes in `update_plan`

Analyzed `server.rs` to extract patterns:
- File layout functions (`task_file_path`, `note_file_path`) use subdirectories
- `normalize_id` strips prefixes on input
- `parse_plan_frontmatter` handles legacy raw-markdown plans
- `handle_update_plan` validates keys, builds records, then writes sequentially

## Root Cause

**File layout**: Prior `todo-<id>.md` prefix was redundant when files live in entity-specific subdirectories.

**Atomic writes**: Direct `std::fs::write` can leave partial files on crash. No rollback possible.

**Write ordering**: `update_plan` wrote `plan.md` before validating task keys. If validation failed, `plan.md` was already updated with metadata referencing non-existent tasks.

**ID handling**: Display output showed bare IDs, but old input handling required callers to know prefix format.

## Solution

### 1. File Layout with Entity Subdirectories

```
<plan>/
  plan.md           # Plan content with YAML frontmatter
  tasks/
    <id>.md         # Task with YAML frontmatter (no prefix)
  notes/
    <id>.md         # Note with YAML frontmatter (no prefix)
```

**Code:**

```rust
fn tasks_dir(dir: &Path, plan_name: &str) -> PathBuf {
    plan_dir(dir, plan_name).join("tasks")
}

fn task_file_path(dir: &Path, plan_name: &str, id: &str) -> PathBuf {
    tasks_dir(dir, plan_name).join(format!("{}.md", normalize_id(id)))
}

fn notes_dir(dir: &Path, plan_name: &str) -> PathBuf {
    plan_dir(dir, plan_name).join("notes")
}

fn note_file_path(dir: &Path, plan_name: &str, id: &str) -> PathBuf {
    notes_dir(dir, plan_name).join(format!("{}.md", normalize_id(id)))
}
```

### 2. Atomic Writes via Temp-File + Rename

All three entity types use the same pattern:

```rust
fn write_task(dir: &Path, task: &TaskRecord) -> Result<(), String> {
    let tasks = tasks_dir(dir, &task.front.plan);
    std::fs::create_dir_all(&tasks).map_err(|err| err.to_string())?;
    let content = serialize_task(task)?;
    let final_path = task_file_path(dir, &task.front.plan, &task.front.id);
    let tmp_path = final_path.with_extension("tmp");
    std::fs::write(&tmp_path, &content).map_err(|err| err.to_string())?;
    std::fs::rename(&tmp_path, &final_path).map_err(|err| err.to_string())?;
    Ok(())
}

fn write_plan_file(path: &Path, content: &str) -> Result<(), String> {
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, content).map_err(|err| err.to_string())?;
    std::fs::rename(&tmp_path, path).map_err(|err| err.to_string())?;
    Ok(())
}

fn write_note(dir: &Path, plan_name: &str, note: &NoteRecord) -> Result<(), String> {
    // Same pattern as write_task
}
```

### 3. Write Ordering in `update_plan`

Validate keys → build all task records → write plan.md → write tasks:

```rust
async fn handle_update_plan(&self, params: UpdatePlanParams) -> Result<CallToolResult, ErrorData> {
    // ... parse existing plan ...

    // 1. Validate all keys before any writes
    let task_specs = params.tasks;
    if let Some(ref tasks) = task_specs {
        let existing_tasks = list_tasks(&self.dir, Some(&name), None, None);
        let mut seen_keys: Vec<String> = existing_tasks
            .iter()
            .filter_map(|t| t.front.key.clone())
            .collect();
        for spec in tasks {
            if let Some(ref key) = spec.key {
                if seen_keys.iter().any(|k| k == key) {
                    return Err(ErrorData::invalid_params(
                        format!("key '{}' already exists in plan '{}'", key, name),
                        None,
                    ));
                }
                seen_keys.push(key.clone());
            }
        }
    }

    // 2. Build task records before writing anything
    let mut task_records = Vec::new();
    if let Some(tasks) = task_specs {
        for spec in tasks {
            let id = gen_id();
            task_records.push(TaskRecord { /* ... */ });
        }
    }

    // 3. Write plan.md first
    let serialized = serialize_plan(&record)?;
    write_plan_file(&path, &serialized)?;

    // 4. Write tasks
    for task in task_records {
        write_task(&self.dir, &task)?;
    }

    result_text(format!("updated plan {}", name))
}
```

### 4. Legacy Plan Parsing

`parse_plan_frontmatter` returns a default `PlanFrontMatter` for raw-markdown plans (no `---` block):

```rust
fn parse_plan_frontmatter(
    content: &str,
    plan_name: &str,
) -> Result<(PlanFrontMatter, String), String> {
    let Some(rest) = content.strip_prefix("---\n") else {
        // Legacy plan: no YAML frontmatter, return defaults
        return Ok((
            PlanFrontMatter {
                id: plan_name.to_string(),
                created_at: "".to_string(),
                ..Default::default()
            },
            content.to_string(),
        ));
    };
    let Some((front, body)) = rest.split_once("\n---\n") else {
        return Err("missing YAML front matter terminator".to_string());
    };
    let front = serde_yaml::from_str(front).map_err(|err| err.to_string())?;
    Ok((front, body.to_string()))
}
```

### 5. ID Normalization on Input

`normalize_id` strips `task-`/`TASK-`/`note-`/`NOTE-` prefixes for robustness:

```rust
fn normalize_id(id: &str) -> String {
    let trimmed = id.trim();
    let trimmed = trimmed
        .strip_prefix("task-")
        .or_else(|| trimmed.strip_prefix("TASK-"))
        .or_else(|| trimmed.strip_prefix("note-"))
        .or_else(|| trimmed.strip_prefix("NOTE-"))
        .unwrap_or(trimmed);
    trimmed.to_ascii_lowercase()
}
```

Display output uses bare IDs:

```rust
fn display_id(id: &str) -> &str {
    id
}
```

### 6. `get_task` Dual Lookup

Accepts `id` for global lookup OR `key`+`plan` for scoped lookup:

```rust
async fn handle_get_task(&self, params: GetTaskParams) -> Result<CallToolResult, ErrorData> {
    let task = if let Some(id) = params.id {
        read_task_by_id(&self.dir, &id)?
    } else if let (Some(key), Some(plan)) = (params.key, params.plan) {
        let plan_name = normalize_plan_name(&plan);
        let tasks = list_tasks(&self.dir, Some(&plan_name), None, None);
        tasks.into_iter()
            .find(|task| task.front.key.as_deref() == Some(key.as_str()))
            .ok_or_else(|| ErrorData::invalid_params(
                format!("task not found for key '{}' in plan '{}'", key, plan_name),
                None,
            ))?
    } else {
        return Err(ErrorData::invalid_params(
            "provide either 'id' or both 'key' and 'plan'".to_string(),
            None,
        ));
    };
    // ... return task JSON ...
}
```

## Why This Works

**Entity subdirectories**: `<plan>/tasks/<id>.md` and `<plan>/notes/<id>.md` disambiguate entity type by location, not filename prefix. Cleaner API, easier enumeration.

**Atomic writes**: `write(tmp)` + `rename(tmp, final)` is atomic on POSIX systems. Either the old file remains intact, or the new file is fully written. No partial state.

**Write ordering**: Validating all keys before writing `plan.md` ensures `plan.md` never references tasks that failed validation. Pre-building records means I/O only happens after all in-memory construction succeeds.

**Legacy parsing**: Plans created before the rewrite work without modification. The default `PlanFrontMatter` provides sensible defaults.

**ID normalization**: Callers can pass `task-abc123` or `NOTE-ABC123` and it works. Display shows bare IDs, so output is clean.

**Dual lookup**: `get_task` absorbs both global ID lookup and scoped key lookup, replacing the old `plan_get_todo` pattern with a single tool.

## Prevention Strategies

**Test Cases:**

- `update_plan_batch_creates_tasks` — verify batch creation writes both tasks
- `update_plan_batch_rejects_duplicate_key` — verify key uniqueness enforced before writes
- `list_tasks_by_tag`, `list_tasks_across_plans` — verify cross-plan enumeration
- `parse_plan_frontmatter` legacy case — raw markdown returns default frontmatter
- Atomic write recovery — kill process during write, verify no partial file

**Code Review Checklist:**

- [ ] Does batch operation validate all state before any writes?
- [ ] Are records pre-built before I/O?
- [ ] Does write function use temp-file + rename?
- [ ] Does ID input normalization handle case and prefix variants?
- [ ] Does legacy parsing return defaults for raw markdown?

## Related Issues

- **Plan:** harnx-mcp-plans-revamp
- **Prior Solution:** [logic-errors/mcp-todo-fs-restructure-2026-05-01.md](mcp-todo-fs-restructure-2026-05-01.md) — established `todo-<id>.md` pattern and YAML frontmatter for `harnx-mcp-todo`
- **Prior Solution:** [integration-issues/plan-note-file-storage-2026-05-03.md](../integration-issues/plan-note-file-storage-2026-05-03.md) — established `note-<id>.md` files and JSON overview for `read_plan`
