---
harnx: patch
---

Bridged MCP tools now keep their `_meta.call_template` and `_meta.result_template`, so custom tool call/result templates render again for tools reached through `harnx-mcp-bridge` (#1349). `ToolSpec` gained an optional `meta` field carrying the tool's `_meta`, and the bridge, toolset-server adapter, and NATS tool provider thread it end to end.
