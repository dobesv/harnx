---
harnx: minor
---
Add a connection-local MCP handler that lazily reserves tools, advertises only selected provider-backed declarations, and routes calls through the same package-aware snapshot. Refresh catalogs after reservation recovery, preserve MCP text and media results, and cancel outstanding calls before releasing a connection's reservation.
