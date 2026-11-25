---
harnx: patch
---
harnx-mcp-server no longer loses the cancellation of an in-flight tool call when its stdio client disconnects: an aborted tool call now publishes its cancel before it returns, instead of leaving it to a background task that died when the process exited.
