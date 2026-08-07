---
harnx: patch
---
Update `rmcp` to 3.x.

The MCP server trait now returns `CallToolResponse`, an enum whose other variants cover elicitation and long-running tasks. Every server here answers in one step, so tool dispatch moved to its own method returning a plain `CallToolResult` and `call_tool` converts. `ListToolsResult` gained the SEP-2549 caching fields and is built with `ListToolsResult::with_all_items`, which fills them in and marks the result complete. `Meta` is now `MetaObject`, and `StreamableHttpServerConfig::with_stateful_mode` is `with_legacy_session_mode`.
