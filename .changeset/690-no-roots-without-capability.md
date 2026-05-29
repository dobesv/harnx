---
harnx: patch
---
MCP servers (`harnx-mcp-fs`, `harnx-mcp-bash`) no longer send `roots/list` requests to clients that did not advertise the `roots` capability (#690). Such clients can't answer the request, so the servers now keep their CLI-provided roots instead.
