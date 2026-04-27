---
harnx: minor
---
Add git-backed local history and rollback to harnx-mcp-fs and harnx-mcp-bash. Each mutating tool call (write_file, edit_file, exec, spawn/wait) now snapshots affected git repositories before and after, includes a unified diff in the tool response, and supports rollback via the new rollback_file tool.
