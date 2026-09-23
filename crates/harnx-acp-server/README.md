# harnx-acp-server

ACP (Agent Client Protocol) server front-end for harnx agents.

## Overview

`harnx-acp-server` exposes harnx agents via the Agent Client Protocol (ACP v1) over stdio. It allows IDE clients like Zed and JetBrains (WebStorm, Air) to communicate with harnx agents using a standard JSON-RPC protocol.

## Status

**Phase 1 (current)**: Protocol handshake scaffold.
- `initialize` — negotiates protocol version 1, advertises minimal capabilities.
- `session/new` — returns a session ID (in-memory, no NATS binding).
- All logging → stderr; stdout carries only protocol frames.

Phase 2+ will add NATS binding, prompt execution, event streaming, and permission handling.

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
- `authenticate` — No-op in this phase (placeholder for future auth).
- `session/new` — Creates a new session and returns its ID.

## Client Configuration

### Zed

In `~/.config/zed/settings.json`:

```json
{
  "agent_client_protocol_servers": {
    "harnx": {
      "command": "/path/to/harnx-acp-server",
      "args": ["--agent", "default"]
    }
  }
}
```

### JetBrains (WebStorm / Air)

In `~/.jetbrains/acp.json`:

```json
{
  "harnx": {
    "command": "/full/path/to/harnx-acp-server",
    "args": ["--agent", "default"]
  }
}
```

Note: Use the absolute path to the binary. JetBrains GUI PATH is restricted.

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
