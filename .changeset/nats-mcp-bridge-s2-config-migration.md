---
harnx: minor
---

feat(nats): migrate roots-free MCP servers (`context7`, `exa`, `fetch`, `grep`, `plans-github`, `wet`, `dev`) to run over NATS via `harnx-mcp-bridge`. Their configurations move from `mcp_servers/` to `tool_servers/`; `fs` and `bash` remain stdio pending roots support. References #1224.
