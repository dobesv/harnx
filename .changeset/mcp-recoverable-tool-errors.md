---
harnx: minor
---

MCP servers now return recoverable tool failures and argument validation errors as `CallToolResult` with `is_error: true` instead of JSON-RPC protocol error frames (`Err(ErrorData)` / `McpError`), allowing client agents to self-correct without terminating the session (#1862).

Note for client authors (such as kagent or other MCP client consumers): domain failures and argument validation errors are now returned as `CallToolResult` with `is_error: true` rather than JSON-RPC error frames (`Err(ErrorData)` / `McpError`). Client agents receive the error as tool result content and can self-correct instead of encountering a fatal protocol exception. JSON-RPC error frames are reserved for protocol violations, unknown tool names, and broken transport state.
