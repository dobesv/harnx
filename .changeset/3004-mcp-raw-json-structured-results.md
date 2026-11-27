---
harnx: patch
---

Honor a structured-result request when the MCP toolset adapter wraps a native tool's raw JSON: the result now carries the tool's JSON in `structuredContent` (with its compact form as text), so `-o json` through a stdio MCP bridge prints the tool's raw JSON, and Python `tools.*` calls match native output for object results. A serialized `CallToolResult` still passes through unchanged, and a caller that did not ask for a structured result keeps the pretty-text envelope.
