---
harnx: minor
---
Serve selected harnx tools over MCP stdio and stateful Streamable HTTP at `/mcp`. Give each HTTP session its own backing harnx session, cancel pending calls on EOF, DELETE, or inactivity expiry, and stop reservation renewal before release. HTTP retains rmcp's five-minute inactivity timeout and requires a stateful MCP protocol version (2025-11-25 or earlier).
