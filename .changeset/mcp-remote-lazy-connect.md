---
harnx: patch
---
harnx-mcp-remote now connects upstream on the first request when clients skip initialize, so harnx-mcp-bridge can wrap it. Concurrent requests share one connection attempt, failed attempts can retry, and shutdown cancels stalled handshakes.
