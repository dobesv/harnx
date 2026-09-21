# harnx-time-tools

`harnx-time-tools` is a native toolset server providing time and timezone utilities. It runs in NATS mode by default and supports `--mcp-stdio` for MCP backward compatibility.

## Installation

From the repo root:

```bash
cargo install --path crates/harnx-time-tools
```

## Tools

| Tool | Description |
| :--- | :--- |
| `get_current_time` | Get the current time in a specified timezone (default: local). |
| `wait` | Wait for a specified duration (max 3600 seconds). |
| `wait_until` | Wait until a specified timestamp. |
| `convert_time` | Convert a timestamp between timezones. |

## CLI Options

| Option | Description |
| :--- | :--- |
| `--mcp-http` | Serve MCP over Streamable HTTP at `/mcp`. |
| `--host <ADDR>` | Bind address for HTTP mode (default: `0.0.0.0`). |
| `--port <N>` | Bind port for HTTP mode (default: `3001`). |
| `--mcp-stdio` | Run in stdio MCP mode instead of NATS. Required when launching behind `harnx-mcp-bridge`. |
| `--metrics-addr <ADDR>` | Serve Prometheus metrics at http://ADDR/metrics. |
| `--healthz-addr <ADDR>` | Serve readiness checks at http://ADDR/healthz. |
| `--help`, `-h` | Show help. |

## Native configuration

Run the server directly from a `tool_servers/time.yaml` configuration:

```yaml
command: harnx-time-tools
description: Time and timezone utilities
```

## Stdio MCP mode

To run behind `harnx-mcp-bridge` for stdio MCP compatibility, the wrapped server must pass `--mcp-stdio`:

```yaml
command: harnx-mcp-bridge
args:
  - --name
  - time
  - --
  - harnx-time-tools
  - --mcp-stdio
```

Without `--mcp-stdio`, the native server defaults to NATS mode and the bridge handshake times out.
