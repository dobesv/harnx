---
harnx: minor
---

feat(nats): add `harnx-mcp-bridge`, a generic MCP→NATS bridge that wraps any stdio MCP server and re-exposes its tools over NATS. Migrate the `plans` tool server to run over NATS via the bridge; the `harnx-mcp-plans` binary still works standalone as an MCP server (`--mcp-stdio`) for external MCP clients. References #1224.
