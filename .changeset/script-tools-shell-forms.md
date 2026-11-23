---
harnx: minor
---
A script tool's Bash body now gets the same shell forms as a `bash` tool script.
In an existing body, `fs_read` prints the file's text like `cat` where it printed numbered lines, `fs_ls`, `fs_find` and `fs_grep` print plain lines, `fs_grep` exits 1 when nothing matches (under `set -e`, use it in `if`, with `||`, or add `|| true`), and `-o json` prints the tool's fields. Python bodies are unchanged.
