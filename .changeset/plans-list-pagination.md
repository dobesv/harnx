---
harnx: patch
pantheon: patch
---

Bound plan, task, and note listings to 20 items by default (maximum 100), return `{items, next_cursor}` for key-based pagination, and fetch only page document bodies. Plan listings default to the caller's session; use `all_sessions: true` to list every session. Count tasks and notes from live keys and renew activity only for returned plans. Document cursor handling and filtered task pages in agent instructions.
