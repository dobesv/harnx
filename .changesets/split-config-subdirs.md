---
harnx: minor
---
Split `clients`, `mcp_servers`, and `acp_servers` out of `config.yaml` into separate per-entry YAML files in `clients/`, `mcp_servers/`, and `acp_servers/` subdirectories under the config directory. The file stem (filename without `.yaml`) is used as the server/client name, so no `name:` field is required in MCP/ACP server files. All agents defined in `agents/<name>.md` are now automatically registered as ACP servers (using the current binary with `--acp <name>`), so manual `acp_servers` entries are only needed to override defaults. Closes #258, #160.
