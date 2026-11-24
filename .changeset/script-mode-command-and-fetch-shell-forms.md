---
harnx: minor
---
Inside a `bash` script, `bash_exec`, command templates and the fetch tools now print like shell commands, and tools behind `harnx-mcp-bridge` or in the k8s sandbox get their shell forms too.
`bash_exec` and non-jsonl command templates print the command's stdout and stderr separately and exit with its status like `bash -c` (124 for its own timeout, 125 when it could not start, 130 when a new message stopped it). `bash_spawn`, `bash_wait`, `bash_status` and `bash_read_exec_log` report in the same style. The fetch tools print the page text with notes about cut-off output on stderr. Direct tool calls are unchanged apart from the signal status below.
A script tool's Bash body gets these forms too, so a failing `bash_exec` there ends the body under `set -e` unless it is guarded. `bash_read_exec_log` has a new `line_numbers` parameter. A command killed by a signal under the sandbox now reports 128 plus the signal number instead of 1, in direct calls too.
