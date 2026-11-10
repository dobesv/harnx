---
harnx: patch
---

Deleting a session no longer leaves a purge marker in the tool invocation journal for every tool call the session made. Rows are removed with one filtered stream purge, and deletion no longer scans the whole bucket for rows in the layout used before October 2026; those rows, and markers left by earlier deletions, stay until the `harnx_tool_invocations` bucket is recreated.
