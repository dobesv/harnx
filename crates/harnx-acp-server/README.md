# harnx-acp-server

ACP (Agent Client Protocol) server front-end for harnx agents.

## Overview

`harnx-acp-server` exposes harnx agents via the Agent Client Protocol (ACP v1) over stdio. It allows IDE clients like Zed and JetBrains (WebStorm, Air) to communicate with harnx agents using a standard JSON-RPC protocol.

## Status

**Current ACP v1 bridge**: NATS-backed prompt turns with durable session loading, event fidelity, cancellation, tool permission requests, and safe committed-handoff fallback.
- `initialize` — negotiates protocol version 1 and advertises minimal capabilities.
- `session/new` — creates a NATS-backed harnx session and accepts IDE-injected MCP server entries.
- `session/load` — replays one scoped durable NATS transcript as ordered ACP updates.
- `session/prompt` — streams ordered assistant, thought, tool-call, tool-result, notice, and flagged model-error updates.
- `session/cancel` — stops the local prompt follower and durably cancels the NATS turn.
- A committed handoff reports the target agent, local session ID, cluster, and opening instructions.
- All logging goes to stderr; stdout carries only protocol frames.

### Session loading

`session/load` is advertised through `agentCapabilities.loadSession`. A load
resolves the requested local session ID only under this server's configured
cluster and agent, reads one durable transcript snapshot, and replays user,
assistant, and tool entries in order. Durable control records such as turn-end,
handoff, and approval markers remain silent.

Loaded sessions are read-only snapshots in this phase. Loading does not attach
a live event subscription or make the loaded ID available to `session/prompt`;
load again to include entries committed after the prior snapshot. Session
list, resume, close, and delete remain unsupported and unadvertised.

### Handoff limitation

ACP v1 has no agent-initiated session-switch method. ACP clients therefore do
not auto-follow handoffs yet. When harnx commits a handoff, its worker has
already created and enqueued the target session. The bridge reports that
independently running target as an ordinary agent message and marks the source
ACP session inactive; another prompt to the source returns an actionable error
instead of continuing the old conversation.

Open the target in the TUI with the exact `.session <agent> <session-id>`
command shown in the fallback. For Web, start
`harnx-serve --addr 127.0.0.1:8000`, open `http://127.0.0.1:8000/`, and
select the reported agent and session. Phase 7 will add full handoff following
while keeping the same ACP session.

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
- `session/load` — Replays a scoped durable transcript snapshot.
- `session/prompt` — Runs a turn and streams assistant text updates.
- `session/request_permission` — Requests a per-turn allow or reject decision for gated tools.
- `session/cancel` — Cancels an in-flight turn, including a pending permission request.

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

Replace `/home/you/.cargo/bin/harnx-acp-server` with an absolute path.

### MCP toolsets supplied by IDE clients

Zed, WebStorm, and Air may inject client MCP servers into
`session/new.mcpServers`. The ACP server accepts these fields for protocol
compatibility, but it does not launch or route tools through those client MCP
servers. Harnx executes turns with backend toolsets from its own agent
configuration.

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
