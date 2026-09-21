---
harnx: major
---

Standardize MCP Streamable HTTP options across the `time`, `bash`, `fs`, `grep`, and `plans` tool servers. Each server now accepts `--mcp-http`, `--host`, and `--port`, with documented default ports. The plans server no longer accepts `--http`, which is a breaking change for users of that flag.

The MCP adapter now reports unknown tools as JSON-RPC `-32602 invalid_params` instead of `method_not_found`.
