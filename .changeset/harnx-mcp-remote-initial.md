---
"harnx": minor
---

feat(mcp-remote): add stdio→HTTP MCP proxy binary

Add `harnx-mcp-remote` — a stdio-based MCP proxy for remote HTTP MCP servers. Supports forwarding negotiated MCP capabilities such as tools, prompts, and resources over both streamable HTTP (MCP 2025-03) and legacy SSE (MCP 2024-11) transports via rmcp's unified StreamableHttpClientTransport. Auth via bearer token, custom headers, and mTLS, configurable through CLI flags and most settings via env vars.
