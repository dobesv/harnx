---
harnx: minor
coding: patch
pantheon: patch
---

Add optional nested `wait` configuration to `bash_spawn` with duration and output-filtering controls, preserving process lifecycle on wait expiry and updating eligible agent guidance.

- `bash_spawn` accepts an optional nested `wait` object with `timeout_secs`, `head_lines`, `tail_lines`, `max_output_bytes`, and `grep`.
- Omitting `wait` preserves immediate spawn behavior. `wait: {}` defaults to 120 seconds, and `wait.timeout_secs: 0` polls current status immediately. Wait expiry returns partial output without terminating the running process.
- Recommends bounded waits under 5 minutes to stay within common LLM prompt cache TTLs, while supporting explicit longer durations.
- Shipped `coding` and `pantheon` agents with the full shell execution trio (`bash_spawn`, `bash_wait`, `bash_terminate`) now prefer `bash_spawn` with nested `wait` for tests, linters, and builds, with `bash_wait` or `bash_terminate` for post-expiry management. `bash_exec` remains available for quick synchronous commands.
- MCP tool listings now carry each tool's request-timeout policy in `_meta` (`timeout_secs`): `harnx-mcp-bridge` copies it into `ToolSpec.timeout_secs` for its NATS registration, and `harnx_toolset_server`'s MCP adapter writes it from native specs. Bash `spawn`/`wait`/`exec` advertise `0` (no elapsed deadline) on this route, matching the native NATS toolset, so a bridged nested spawn wait (`wait: {}`, 120-second default) returns its first execution_id-bearing response instead of expiring at the caller's 60-second request backstop. Third-party MCP servers that advertise nothing keep the caller's default backstop; malformed values are ignored rather than disabling a deadline. Direct stdio clients are unaffected.
