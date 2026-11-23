---
harnx: minor
---
Make the file tools behave like shell commands inside `bash` scripts: `fs_read` prints plain file text like `cat`, `fs_ls`, `fs_find` and `fs_grep` print one entry, path or `path:line:text` row per line, notes about cut-off results go to stderr, and `fs_grep` exits 1 when nothing matched.
Tools declare their shell form with `ToolSpec::with_script_output`, and `fs_read` gains a `line_numbers` parameter (default on for direct calls, off in scripts). Direct calls from a model are unchanged.
A tool command the script layer refuses now exits 126 instead of 3, 4 or 5, with a `needs approval:`, `blocked:` or `rejected:` message on stderr.
