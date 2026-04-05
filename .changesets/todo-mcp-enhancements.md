---
harnx: minor
---

Enhanced the todo MCP server with key-based identification and dependency tracking:

- Added `key` field to todos - a unique identifier within a plan (e.g., "task-1", "api-setup")
- Added `dependencies` field - a list of keys this todo depends on within the same plan
- Added `todos` parameter to `write_plan` - create todos along with the plan in a single request
- Added `plan_get_todo` tool - fetch a todo by plan name and key instead of todo ID
- Updated `todo_create` and `todo_update` to support key and dependencies fields