---
harnx: minor
---
Replace PID-based identifiers in the bash MCP server with stable per-execution IDs. `spawn` now returns an `execution_id`; `wait`, `terminate`, and `read_exec_log` all accept `execution_id`. Stdout and stderr are kept in separate log files for both `exec` and `spawn`. `wait` now accepts the same output-filtering parameters as `exec` and returns the same response format. Output blocks now show stdout and stderr in clearly demarcated separate sections (`===== stdout =====` / `===== /stdout =====`), each truncated independently, so neither stream's content is lost to the other during truncation.
