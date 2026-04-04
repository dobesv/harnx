---
harnx: minor
---
Add idle-aware timeout for ACP sub-agent operations and propagate all session update variants. The ACP client now uses dual timeouts: `idle_timeout_secs` (default 300s) resets on each notification, and `operation_timeout_secs` (default 3600s) is the absolute maximum. All `SessionUpdate` variants (`AgentThoughtChunk`, `ToolCall`, `ToolCallUpdate`, `Plan`) are now forwarded, ensuring sub-sub-agent activity keeps the connection alive. Truncation messages in MCP tools now include parameter hints (e.g. `Use max_output_bytes to increase limit`).
