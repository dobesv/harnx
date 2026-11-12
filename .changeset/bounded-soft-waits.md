---
harnx: minor
pantheon: minor
coding: minor
---

Waits on sub-agents and background commands now default to and cap at 240 seconds, accept `forever: true`, and return early when a new message arrives.

Breaking argument changes:

- Every wait takes `seconds` (1 to 240, default 240) or `forever: true`, not both. `0` and values above 240 are rejected.
- `bash_spawn`'s nested `wait.timeout_secs` is now `wait.seconds`, and `bash_wait`'s `timeout_secs` is now `seconds`. The old names are rejected.
- `session_wait` takes `seconds`, `forever`, `include_partial` and `read` at the top level; the nested `wait` object is removed.
- `bash_spawn`, `bash_exec` and command templates take `timeout_secs` from 1 to 86400, and 86400 (24 hours) is the default for all three. `0` no longer means no deadline. A spawn killed at its deadline reports `status: timed_out`.
- `bash_spawn`, `bash_wait` and `bash_status` now reject any argument they do not declare instead of ignoring it, which matters for third-party MCP clients of `harnx-bash-tools`.
- `status` is now a built-in bash tool, so a command template cannot use that name or any other built-in name. A template passed with `--tool` or `--tools-dir` fails startup, and an auto-discovered one is skipped with a warning. The `--enable-tool` example in `docs/bash-mcp-server.md` now names a template `service_status`; a config that used `--enable-tool status` for its own template must rename it.

New tools and results:

- `bash_status` reports a spawned command's state without waiting. For a sub-agent, call `session_load` with `prompt_id`.
- A wait can end early with `stopped_by: "new_message"` (a new message arrived for the waiting agent) or `stopped_by: "caller_deadline"` (60 seconds before the agent's own run deadline). Neither is an error. Waits over the k8s-sandbox proxy and MCP stdio routes run to their limit.
- `session_wait` no longer reports `unknown` after a child compacts mid-turn.
