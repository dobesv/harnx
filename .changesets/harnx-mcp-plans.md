---
harnx: minor
---
Replace `harnx-mcp-todo` with `harnx-mcp-plans`: a fully rewritten file-based plan/task/note management MCP server.

**New tools (15):** `list_plans`, `add_plan`, `get_plan`, `update_plan`, `delete_plan`, `list_tasks`, `add_task`, `get_task`, `update_task`, `append_task`, `delete_task`, `list_notes`, `add_note`, `get_note`, `delete_note`

**Breaking changes:**
- Binary renamed: `harnx-mcp-todo` → `harnx-mcp-plans`
- All tool names changed (old `todo_*`/`read_plan`/`write_plan` tools removed)
- Data directory default: `.agent/todos/` → `.agent/plans/`
- File layout: `<plan>/tasks/<id>.md` and `<plan>/notes/<id>.md` (was `<plan>/todo-<id>.md` and `<plan>/note-<id>.md`)
- Config: `example_config/mcp_servers/todo.yaml` → `plans.yaml`

**New features:**
- YAML front-matter on plan files and note files (was raw markdown)
- New metadata fields: `summary`, `author`, `assignee`, `executor` on tasks/plans; `git_branch`, `github_owner_repo` on plans
- `get_task` accepts `key`+`plan` for scoped lookup (absorbs old `plan_get_todo`)
- `add_plan` creates plans explicitly; `update_plan` implicitly creates if missing
- Atomic file writes (temp+rename) on all entities
- Task key uniqueness enforced within a plan across all write paths
