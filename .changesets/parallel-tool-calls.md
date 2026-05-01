---
harnx: minor
---
Parallel tool call execution: when the LLM returns multiple tool calls, MCP dispatch now runs concurrently (via `futures_util::future::join_all`) instead of sequentially. Pre-flight (argument parsing, `emit_tool_call_fn`, PreToolUse hooks, user confirmation) and post-processing (PostToolUse hooks, result emission) remain sequential. Result order is preserved. References GitHub issue #380.
