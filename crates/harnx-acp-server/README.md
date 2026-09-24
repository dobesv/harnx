# harnx-acp-server

ACP (Agent Client Protocol) server front-end for harnx agents.

## Overview

`harnx-acp-server` exposes harnx agents via the Agent Client Protocol (ACP v1) over stdio. It allows IDE clients like Zed and JetBrains (WebStorm, Air) to communicate with harnx agents using a standard JSON-RPC protocol.

## Status

**Phase 3**: NATS-backed prompt turns with ACP event fidelity.
- `initialize` — negotiates protocol version 1 and advertises minimal capabilities.
- `session/new` — creates a NATS-backed harnx session and accepts IDE-injected MCP server entries.
- `session/prompt` — streams ordered assistant, thought, tool-call, tool-result, notice, and flagged model-error updates.
- `session/cancel` — stops the local prompt follower and durably cancels the NATS turn.
- All logging goes to stderr; stdout carries only protocol frames.

Later phases add permission handling, persistence, and handoffs.

## Installation

```bash
cargo install --path crates/harnx-acp-server
```

## Usage

```bash
harnx-acp-server --agent <agent-name>
```

Options:
- `--agent, -a`: Agent name to expose (default: `default`).
- `--log-level, -l`: Log level — trace, debug, info, warn, error (default: info).

The server communicates over stdin/stdout using newline-delimited JSON-RPC 2.0. All logs are written to stderr to avoid contaminating the protocol stream.

## Protocol

Implements ACP v1 as defined by the [Agent Client Protocol specification](https://agentclientprotocol.com/).

Supported methods:
- `initialize` — Negotiates protocol version and exchanges capabilities.
- `authenticate` — No-op placeholder for future authentication.
- `session/new` — Creates a NATS-backed session and returns its ID.
- `session/prompt` — Runs a turn and streams assistant text updates.
- `session/cancel` — Cancels an in-flight turn.

## Client Configuration

Find the executable before editing either client configuration:

```bash
command -v harnx-acp-server
```

Copy that absolute path into the `command` field below. Don't use only
`harnx-acp-server`: GUI applications often start with a restricted `PATH`.

### Zed

Add a custom agent under `agent_servers` in Zed's `settings.json` (normally
`~/.config/zed/settings.json` on Linux and macOS):

```json
{
  "agent_servers": {
    "harnx": {
      "type": "custom",
      "command": "/home/you/.cargo/bin/harnx-acp-server",
      "args": ["--agent", "default"],
      "env": {}
    }
  }
}
```

Replace `/home/you/.cargo/bin/harnx-acp-server` with the absolute path printed
by the command above. Start a new external-agent thread and select `harnx`.

### JetBrains (WebStorm / Air)

Add the agent to `~/.jetbrains/acp.json`:

```json
{
  "default_mcp_settings": {
    "use_custom_mcp": true,
    "use_idea_mcp": true
  },
  "agent_servers": {
    "harnx": {
      "command": "/home/you/.cargo/bin/harnx-acp-server",
      "args": ["--agent", "default"],
      "env": {}
    }
  }
}
```

Replace `/home/you/.cargo/bin/harnx-acp-server` with an absolute path. WebStorm
and Air can pass configured or integrated IDE MCP servers in `session/new`;
the ACP bridge accepts those entries while harnx continues to use tool servers
from its own agent configuration.

## Development

Run tests:
```bash
cargo nextest run -p harnx-acp-server
```

Run with full CI profile (includes integration tests from other crates):
```bash
cargo nextest run --all --profile ci
```

## License

Apache-2.0 or MIT, at your option.
