---
harnx: minor
---

Serialize concurrent mutations in the filesystem MCP server to prevent corruption from parallel edits. Same-file edits (write, edit, insert, re_replace) are now serialized via per-file locks, while `rollback_file` takes an exclusive repository-wide lock so it cannot interleave with concurrent edits to other files in the same repository.
