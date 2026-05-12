---
harnx: minor
---
Make `insert_line` optional in the `fs_insert` MCP tool. When omitted, text is appended to the end of the file, making it easy to build up large files in chunks without needing to know the current line count.
