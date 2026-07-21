---
"harnx": minor
---

feat(mcp-remote): add stdio→HTTP MCP proxy binary

Add `harnx-mcp-remote` — a stdio-based MCP server that transparently proxies all MCP traffic to a remote HTTP MCP server. Supports both streamable HTTP (MCP 2025-03) and legacy SSE (MCP 2024-11) transports via rmcp's unified StreamableHttpClientTransport. Auth via bearer token, custom headers, and mTLS, configurable through CLI flags or env vars.
