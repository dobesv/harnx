---
harnx: minor
---
Release archives now include the MCP server binaries (`harnx-mcp-bash`,
`harnx-mcp-fs`, `harnx-mcp-plans`, `harnx-mcp-time`) alongside `harnx`,
`harnx-serve`, and `harnx-acp-server`. The example MCP server configs
under `example_config/mcp_servers/` reference these by command name, so
shipping them lets users adopt those configs out of the box without a
local cargo build.
