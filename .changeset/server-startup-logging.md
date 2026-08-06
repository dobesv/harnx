---
harnx: patch
---
Spawned server processes — tool servers, hook servers and the MCP bridge — now install a logger, so the diagnostics they already emit reach the worker log instead of being discarded. The MCP bridge forwards its wrapped child's stderr line by line, which previously went nowhere: a `context7` or `exa` server failing on a missing API key produced no output anywhere. The bridge also logs the command it is starting and the tool count once ready, so a server still initialising is identifiable rather than silent. Set `HARNX_LOG_LEVEL=debug` to see the wrapped child's own output.

A tool server that has not registered but whose process is still running is now reported as possibly still starting, naming the log to look in, rather than as having failed to start.
