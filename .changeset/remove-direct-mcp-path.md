---
harnx: minor
---

Remove the direct MCP integration path. External MCP servers are now added only as `tool_servers/` entries that run through `harnx-mcp-bridge`; the old top-level `mcp_servers/` config directory, `McpManager`, and the `harnx-mcp` crate are gone. Existing `mcp_servers/*.yaml` files are no longer loaded — declare external stdio MCP servers as `tool_servers/*.yaml` launching `harnx-mcp-bridge` instead (see the configuration guide). Tool call/result templates (`_meta.call_template`/`_meta.result_template`) are preserved for bridged tools.
