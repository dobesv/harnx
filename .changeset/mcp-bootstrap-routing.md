---
harnx: minor
---
Validate the MCP server's selected NATS cluster at startup and share local worker routing across connections. Explicit cluster selection overrides frontend environment routing for reservations, discovery, and calls; an explicit config directory also reaches the local child worker.
