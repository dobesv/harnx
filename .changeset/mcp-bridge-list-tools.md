---
harnx: minor
---
`harnx-mcp-bridge --list-tools -- <command>` starts a wrapped MCP server, prints the tools it advertises, and exits without touching NATS. Reaching the listing proves the child spawns, completes the MCP handshake and answers `tools/list`; when it does not, the error distinguishes a child that failed to spawn, one that died during startup, and one that never finished the handshake — three cases a registration timeout in the worker cannot tell apart. `--name` is now only required when actually serving over NATS.
