---
harnx: minor
---
The bash tool's `exec`, `spawn`, `wait` and `status` and the file tools' `write`, `edit`, `insert` and `re_replace` take a `diff` option with the values `none`, `summary`, `files` and `full`, which sets how much of the change they report.

A direct call defaults to `full`, as before. Inside a `bash` script the default is `summary`: one line on stderr with the change's size and the `git diff` command that shows it, where a file write used to print the whole diff on stdout. A script's file writes now print a one-line confirmation on stdout. `none` takes no snapshot, so there is nothing to roll back to. The two ids on a summary line are the snapshots before and after the change, and piping the line's command into `git apply -R` undoes it. `rollback_file` accepts a short commit id of at least 7 hex digits.
