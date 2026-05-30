---
harnx: minor
---

Add opt-in Streamable HTTP transport (MCP spec 2025-03-26) to the `harnx-mcp-time` and `harnx-mcp-plans` servers. Pass `--http` to serve MCP over HTTP at `/mcp` instead of stdio, with `--host` (default `0.0.0.0`) and `--port` (default `3000`) to control binding. Stdio remains the default, so existing usage is unchanged. The plans server's background cleanup loop continues to run in HTTP mode when retention is enabled.
