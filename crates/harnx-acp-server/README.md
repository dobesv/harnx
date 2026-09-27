# harnx-acp-server

ACP (Agent Client Protocol) server front-end for harnx agents.

## Overview

`harnx-acp-server` exposes harnx agents via the Agent Client Protocol (ACP v1) over stdio. It allows IDE clients like Zed and JetBrains (WebStorm, Air) to communicate with harnx agents using a standard JSON-RPC protocol.

## Status

**Current ACP v1 bridge**: NATS-backed prompt turns with durable session loading, event fidelity, cancellation, tool permission requests, and safe committed-handoff fallback.
- `initialize` — Negotiates protocol version 1; advertises `loadSession: true` and `sessionCapabilities: { list: {}, resume: {}, close: {} }`.
- `authenticate` — No-op placeholder responding `Ok` for transport compatibility.
- `session/new` — Creates a NATS-backed session with a real local session ID.
- `session/load` — Replays durable transcript snapshot, establishes live `SessionContext`, and rehydrates handoff state.
- `session/resume` — Establishes live `SessionContext` without replaying history notifications.
- `session/list` — Discovers pinned-agent sessions on the configured cluster, ordered newest-first, with process CWD fallback for unrecorded contexts.
- `session/close` — Cancels active turns, removes in-memory context, and preserves durable history, attachments, and listing visibility. Idempotent.
- `session/prompt` — Streams in-order assistant text chunks, thought chunks, tool lifecycle updates, notices, and model errors.
  - Supported content blocks: text, embedded text resources, resource links.
  - Unsupported content blocks: images, audio, embedded binary blobs (rejected with explicit errors; never silently dropped).
- `session/cancel` — Stops the local prompt follower and durably cancels the NATS turn and any pending permission request.
- `session/request_permission` — Bridges worker tool confirmation callbacks to client allow/reject single-turn choices.
- Committed handoffs report target agent, session ID, cluster, and instructions to switch agent servers.
- All logging goes to stderr; stdout carries only protocol frames.

See the [ACP v1 Support Matrix](#acp-v1-support-matrix) for capability details, content handling, and deferred feature rationales.

## ACP v1 Support Matrix

The tables and descriptions below document ACP v1 method and capability support in `harnx-acp-server`.

### Supported Surface (Capabilities Advertised & Implemented)

| ACP Method / Capability | Capability Advertised | Status | Description |
|---|---|---|---|
| `initialize` | Protocol `1`, `loadSession: true`, `sessionCapabilities: { list: {}, resume: {}, close: {} }` | Shipped | Negotiates protocol version 1 and exchanges client/agent capabilities. Returns agent implementation name (`harnx`), package version, and agent title. |
| `authenticate` | None (empty response) | Shipped | No-op placeholder responding `Ok` for transport compatibility. |
| `session/new` | Standard core method | Shipped | Creates a new NATS-backed session with a real local session ID (UUID). |
| `session/load` | `agentCapabilities.loadSession: true` | Shipped | Resolves the requested session ID under the pinned agent, replays durable transcripts in order, establishes a live `SessionContext`, and rehydrates handoff deactivation state. |
| `session/resume` | `sessionCapabilities.resume: {}` | Shipped | Validates session ownership for the pinned agent and establishes a live `SessionContext` without replaying history notifications. |
| `session/list` | `sessionCapabilities.list: {}` | Shipped | Discovers sessions for the pinned agent on the configured cluster, ordered newest-first by last activity. Falls back to server process CWD if unrecorded. Supports optional CWD filtering. |
| `session/close` | `sessionCapabilities.close: {}` | Shipped | Cancels active turns (local prompt follower and remote NATS worker) and removes in-memory session context. Preserves durable JetStream transcripts, attachments, and listing visibility. Idempotent. |
| `session/prompt` | Standard core method | Shipped | Streams in-order assistant text chunks, thought chunks, tool lifecycle updates (`ToolCallUpdate`), notices, and model errors. Handles turn completion and stop reasons. |
| `session/cancel` | Standard core method | Shipped | Cancels in-flight prompt turns and any pending tool permission requests across local guards and remote NATS workers. |
| `session/request_permission` | Client capability requested by server | Shipped | Bridges worker tool confirmation callbacks to client single-turn choices (`allow` or `reject`). Denies tool call if the client rejects, disconnects, or errors. |

#### Detailed Handler Behaviors

- **`initialize`**: Negotiates protocol version 1 (always advertises v1 even when a client requests higher versions). Exchanges agent capabilities: advertises `loadSession: true` and `sessionCapabilities: { list: {}, resume: {}, close: {} }`. Returns agent metadata with implementation name `harnx`, crate version, and the pinned agent title.
- **`authenticate`**: Placeholder responding `Ok` with an empty response for clients that send authentication requests over stdio.
- **`session/new`**: Creates a NATS-backed session using a fresh UUID as the local session ID. Accepts optional client CWD and MCP server parameters for schema compatibility.
- **`session/load`**: Resolves the session under the configured cluster and pinned agent. Replays the durable JetStream transcript snapshot into sequential client notifications, establishes a live `SessionContext`, and rehydrates handoff deactivation state so subsequent prompts or cancels work.
- **`session/resume`**: Validates that the requested session belongs to the pinned agent, establishes a live `SessionContext` (including handoff deactivation state), and returns immediately without replaying historical transcript notifications.
- **`session/list`**: Queries the NATS session metadata store for the configured cluster. Returns sessions belonging to the pinned agent, sorted newest-first by last activity. Each item includes the real local session ID, title, timestamp, and CWD (falling back to server process CWD if unrecorded). Supports optional exact CWD filtering.
- **`session/close`**: Cancels active turns across both local prompt follower and remote NATS worker, then drops in-memory session state. Durable transcripts, metadata, attachments, and session listing visibility are preserved. Closing an unknown or already-closed session succeeds idempotently.
- **`session/prompt`**: Validates content blocks and streams assistant text chunks, thought chunks, tool call lifecycle updates (`ToolCallUpdate`), notices, and model errors. Handles turn completion, stop reasons, and error mapping.
- **`session/cancel`**: Stops the local prompt follower and sends a durable cancel signal to the NATS worker turn. Also denies and clears any pending tool permission request.
- **`session/request_permission`**: When a backend tool requires human-in-the-loop approval, the server calls the client's `session/request_permission` method with `allow` (`AllowOnce`) and `reject` (`RejectOnce`) choices. If the client approves, the tool proceeds; if the client rejects, disconnects, or times out, the tool call is denied.

#### Supported Prompt Content Blocks

ACP `session/prompt` requests provide an array of `ContentBlock` items. The server processes blocks in order:

- **`TextContent`**: Text is preserved verbatim in prompt input.
- **`EmbeddedResource` (`TextResourceContents`)**: Rendered into prompt text with explicit URI and MIME provenance delimiters:
  ```
  --- Embedded Resource: <uri> (MIME: <mime>) ---
  <text content>
  --- End Embedded Resource: <uri> ---
  ```
- **`ResourceLink`**: Rendered into prompt text as a reference containing name, URI, and optional metadata:
  ```
  [Resource Link: <name> (uri=<uri>, title=..., description=..., mime=..., size=...)]
  ```

If any content block in the request is unsupported or invalid, the entire prompt request fails immediately with an explicit error before starting a turn.

#### Pinned-Agent Model and Cross-Agent Handoff

Each `harnx-acp-server` process is pinned to a single agent configuration (`--agent <name>`, defaulting to `default`). Session creation, listing, loading, and resumption are strictly scoped to that pinned agent.

ACP v1 has no agent-initiated session-switch method. When a harnx agent commits a handoff to another agent:
1. The backend worker creates and enqueues the target session.
2. The server marks the source ACP session deactivated and emits an assistant message identifying the target agent, local session ID, and cluster.
3. The server instructs the user to switch their IDE configuration to the target agent's ACP server and load the target session ID.
4. Any subsequent `session/prompt` to the deactivated source session fails fast with an actionable error directing the user to the target agent's server and session.

To follow a handoff:
1. Switch your IDE to the external agent configuration pointing to the target agent's ACP server.
2. Load or open the reported session ID (using the session picker/list or `session/load`).

Alternatives:
- **TUI**: Run `.session <agent> <session-id>` (displays target agent, session, and cluster).
- **Web UI**: Run `harnx-serve --addr 127.0.0.1:8000`, then select the agent and session.

### Unadvertised and Deferred Features

| Feature / Capability | Status | Technical Rationale | Client Impact / Behavior | Tracking |
|---|---|---|---|---|
| `session/delete` | Unadvertised | Administrative stream deletion races active worker leases. Safe deletion requires a durable worker-owned deletion command. | IDE clients like Zed hide the session delete action when `sessionCapabilities.delete` is absent. | [#2129](https://github.com/dobesv/harnx/issues/2129) |
| Image content (`ContentBlock::Image`) | Unadvertised | Data-URL to NATS CID mapping exists, but the backend worker loop derives input text via `content.to_text()` and rejects turns with empty text when only images are provided. A vision model gating policy is also required. | Image blocks are rejected with an explicit error (`image content blocks are not supported`) before starting a turn to prevent silent content loss. | [#2135](https://github.com/dobesv/harnx/issues/2135) |
| Audio & embedded binary blobs (`AudioContent`, `BlobResourceContents`) | Unsupported | Harnx has no native binary audio or blob model. | Rejected with an explicit error (`audio content blocks are not supported` or `embedded binary blob resources are not supported`) before starting a turn. | None |
| Working directory propagation & `additionalDirectories` | Deferred | NATS worker and tool execution lack a persisted multi-root authorization and confinement contract. Observational CWD is mapped for listing, but execution roots are not dynamically reconfigured by the client. | Server accepts `cwd` and `additionalDirectories` parameters without error for schema compatibility, but executes within the backend worker's environment. | None |
| Session modes & config options | Unadvertised | The server is pinned to a fixed agent configuration. No honest per-session mode switching exists. | IDE clients omit mode selection controls. | None |
| Client-injected MCP servers (`session/new.mcpServers`) | Ignored | Remote NATS workers execute tools within server-side trust boundaries. Bridging client-supplied executable or HTTP MCP configurations across remote workers bypasses server-side tool allowlists and trust boundaries. | Accepted for schema compatibility, but Harnx uses its own configured toolsets. Client-injected MCP servers are not launched or routed. | None |
| Client filesystem & terminal | Unadvertised | Harnx uses backend tools and container sandboxes rather than host IDE tools. | IDE clients do not expose host filesystem or terminal RPCs to the server. | None |
| Authentication (`authenticate`) | Unadvertised | stdio transport relies on ambient local user credentials. Remote network authentication is handled at the transport or broker layer. | Clients skip authentication handshakes and proceed directly to session initialization. | None |

#### Detailed Deferred Feature Rationale

- **`session/delete`** (`sessionCapabilities.delete`): Unadvertised. Direct administrative deletion of streams, KV keys, and attachments can race an active worker holding the session lease. Safe deletion requires a durable worker-owned deletion command that coordinates lease cancellation before storage cleanup. IDEs like Zed hide the delete button when the capability is absent. Tracked in [#2129](https://github.com/dobesv/harnx/issues/2129).
- **Image content** (`ContentBlock::Image`): Unadvertised. While Harnx has internal data-URL and NATS CID attachment plumbing, the worker input pipeline derives turn input through text serialization and rejects turns that contain only images. Vision capability gating and initialize advertisement policies must be implemented together. Prompts containing images are rejected upfront with an explicit error to prevent silent content loss. Tracked in [#2135](https://github.com/dobesv/harnx/issues/2135).
- **Audio and embedded binary blobs** (`AudioContent`, `BlobResourceContents`): Unsupported. Harnx core has no native representation for arbitrary binary audio or blob data. Any prompt containing audio or blob resources is rejected immediately with an explicit error.
- **Working directory execution propagation & `additionalDirectories`**: Deferred. Client requests may provide `cwd` or `additionalDirectories`. Observational CWD is recorded in session metadata for listing, but NATS worker processes and container sandboxes run in their configured worker roots. Propagating client-supplied directory roots requires multi-root authorization, path mapping, and sandbox confinement contracts that are not yet designed.
- **Session modes & configuration options**: Unadvertised. Each server process is pinned to a single agent configuration. Mode switching would imply dynamic capability changes that are unsupported by the pinned-agent architecture. Clients omit mode selector controls when modes are unadvertised.
- **Client-injected MCP servers** (`session/new.mcpServers`): Ignored for security and trust boundary reasons. IDE clients often advertise local MCP tools in `session/new`. Forwarding arbitrary client-specified executables or network endpoints across remote NATS workers bypasses server-side tool policies and authorization boundaries. The server accepts these fields without error for schema compatibility, but relies exclusively on its own configured toolsets.
- **Client filesystem and terminal capabilities**: Unadvertised. Harnx executes tool commands inside containerized backend sandboxes or worker environments, not via reverse RPC calls into the client IDE filesystem or terminal.
- **Authentication**: Unadvertised. Over stdio transport, authentication relies on ambient local user credentials. Network-level authentication is handled by the NATS broker.

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
- `initialize` — Negotiates protocol version 1 and exchanges client/agent capabilities.
- `authenticate` — No-op placeholder responding `Ok` for transport compatibility.
- `session/new` — Creates a NATS-backed session with a real local ID.
- `session/load` — Replays a durable transcript snapshot and establishes live session context.
- `session/resume` — Establishes live session context without replaying history notifications.
- `session/list` — Discovers sessions for the pinned agent, ordered newest-first.
- `session/close` — Cancels active turns and releases in-memory context while preserving durable history.
- `session/prompt` — Streams in-order assistant text, thoughts, tool lifecycle updates, notices, and errors.
- `session/cancel` — Cancels in-flight turns and any pending permission request.
- `session/request_permission` — Bridges worker tool confirmation to client single-turn choices.

See the [ACP v1 Support Matrix](#acp-v1-support-matrix) for advertised capabilities, content block handling, and deferred feature rationales.

## Client Configuration

Find the executable before editing either client configuration:

```bash
command -v harnx-acp-server
```

Copy that absolute path into the `command` field below. Don't use only
`harnx-acp-server`: GUI applications often start with a restricted `PATH`.

Because each ACP server process is pinned to a single agent, configure a separate agent server entry for each agent you want to interact with (for example, `harnx` for the default agent, and `harnx-reviewer` for a review agent).

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
