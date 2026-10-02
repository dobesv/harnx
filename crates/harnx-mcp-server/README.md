# harnx-mcp-server

`harnx-mcp-server` exports harnx tools and agents-as-tools over the Model Context Protocol (MCP) using stdio or Streamable HTTP.

## Overview

External MCP clients (such as Claude Code or Claude Desktop) can use `harnx-mcp-server` to invoke native harnx tools (filesystem, bash, plans, attachments) and run sub-agent turns (`<agent>_session_prompt`, `<agent>_session_new`).

Each MCP connection is backed by an isolated, durable harnx session. The server holds a worker-side **tool reservation** that starts and maintains the selected tool servers for the connection lifetime without running any LLM turn.

## Quickstart

Run over stdio:

```sh
harnx-mcp-server --mcp-stdio --use-tools 'fs_*,bash_exec'
```

Run over Streamable HTTP:

```sh
harnx-mcp-server --mcp-http --port 3010 --use-tools 'fs_*,bash_exec'
```

HTTP endpoint: `http://127.0.0.1:3010/mcp`

### HTTP Defaults and Security Warning

- Default bind host: `127.0.0.1` (loopback only)
- Default bind port: `3010` (chosen above native toolset defaults 3000–3007)

By default, `harnx-mcp-server` binds to `127.0.0.1`, allowing connections only from the local machine. To expose the HTTP endpoint to external callers or across a network, explicitly opt in by passing `--host <address>` (such as `--host 0.0.0.0` or a specific network interface address).

**Caution**: `harnx-mcp-server` provides no built-in client authentication or access control. While `rmcp` validates the HTTP `Host` header against allowed host authorities, **Host header validation is not authentication**—any network client that can reach the port can supply any header. Because exported tools (such as bash and filesystem) can execute arbitrary shell commands and modify files, **never expose non-loopback endpoints without network controls** (such as an authenticating reverse proxy with TLS, a VPN, or firewall rules).

## Client Configuration

### Claude Code

Claude Code supports both stdio and Streamable HTTP MCP servers.

#### stdio

Add via the Claude Code CLI:

```sh
claude mcp add harnx -- harnx-mcp-server --mcp-stdio --use-tools 'fs_*,bash_exec'
```

Or add directly to `~/.claude.json` or your project `.claude.json`:

```json
{
  "mcpServers": {
    "harnx": {
      "command": "harnx-mcp-server",
      "args": [
        "--mcp-stdio",
        "--use-tools",
        "fs_*,bash_exec"
      ]
    }
  }
}
```

#### Streamable HTTP

Add via the Claude Code CLI:

```sh
claude mcp add --transport http harnx http://127.0.0.1:3010/mcp
```

Or add directly to `~/.claude.json` or your project `.claude.json`:

```json
{
  "mcpServers": {
    "harnx": {
      "url": "http://127.0.0.1:3010/mcp"
    }
  }
}
```

### Claude Desktop

Claude Desktop configuration file paths:
- **macOS**: `~/Library/Application Support/Claude/claude_desktop_config.json`
- **Linux**: `~/.config/Claude/claude_desktop_config.json`
- **Windows**: `%APPDATA%\Claude\claude_desktop_config.json`

#### stdio

Add the server under `mcpServers` in `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "harnx": {
      "command": "/path/to/harnx-mcp-server",
      "args": [
        "--mcp-stdio",
        "--use-tools",
        "fs_*,bash_exec"
      ]
    }
  }
}
```

Use the absolute path to `harnx-mcp-server` (find it with `which harnx-mcp-server` or `command -v harnx-mcp-server`). GUI applications often start with a minimal `$PATH` that does not include Cargo's binary directory.

#### Remote / HTTP Limits

Claude Desktop only supports spawning local stdio processes in `claude_desktop_config.json`. It does not support remote HTTP URLs directly.

To connect Claude Desktop to an HTTP instance of `harnx-mcp-server`, use a stdio-to-HTTP proxy like `harnx-mcp-remote`:

```json
{
  "mcpServers": {
    "harnx-remote": {
      "command": "/path/to/harnx-mcp-remote",
      "args": [
        "--url",
        "http://127.0.0.1:3010/mcp"
      ]
    }
  }
}
```

## Tool Selection

Pass tool selectors with `--use-tools` or the `HARNX_MCP_USE_TOOLS` environment variable. The syntax matches agent `use_tools` configuration:

- **Comma-separated**: `--use-tools 'fs_*,bash_exec'`
- **Repeated flags**: `--use-tools 'fs_*' --use-tools 'bash_exec'`
- **Brace expansion**: `--use-tools 'fs_{read,write},bash_{exec,spawn}'` (braces protect enclosed commas from splitting)
- **Wildcard globs**: `--use-tools 'fs_*'` or `--use-tools 'attachments_*'`
- **Toolset aliases**: Configured alias names such as `fs`, `bash`, or `plans`

If both the environment variable and CLI flags are present, CLI values replace the environment variable value. Without `--use-tools`, the environment variable supplies selectors.

### Missing Selectors vs. Zero Matches

- **Missing selectors**: Passing no selectors (or an empty selector string) is a startup error. The server exits with code 1:
  ```text
  Error: no tool selectors specified; set --use-tools or HARNX_MCP_USE_TOOLS
  ```
  There is intentionally no default "expose all" mode.
- **Zero matches**: Selectors that match no registered tools (such as `--use-tools 'nonexistent_*'`) are valid. The server starts successfully and returns an empty tool list to the client.

## Package Context and Tool Naming

Pass `--package <PACKAGE>` or set `HARNX_MCP_PACKAGE` to evaluate tool names from the perspective of an agent in that package:

- Tools inside that package appear with package-unqualified names (e.g. `fs_read`, `fs_write`). Package context strips the package prefix (`pkg__`), not the tool server prefix (`fs_`).
- Tools outside that package appear with namespace qualification (e.g. `pkg__server_tool`).

When `--package` is omitted, tools retain their server-registered names (e.g. `fs_read`, `bash_exec`).

## Backing Sessions and Tool Reservations

Each MCP connection is linked to a dedicated harnx session:

- **Caller identity**: Tools that require session context (`plans_add_plan`, `attachments_attachment_create`, `<agent>_session_new`, `<agent>_session_prompt`) receive valid session metadata and caller credentials.
- **Session retention**: Backing sessions remain durable and are retained in NATS after disconnect. Connection termination (stdio EOF, HTTP `DELETE`, or inactivity timeout) stops the renewal loop, cancels active calls, and releases the worker tool reservation; it does **not** delete the durable backing session.
- **Session GC and cleanup**: Automatic session cleanup is controlled by `HARNX_CLEANUP_REMOTE_SESSIONS_DAYS` (or worker configuration key `cleanup_remote_sessions_days`). When unset or set to `0`, automatic session garbage collection (GC) is **disabled**, and session state in NATS JetStream persists indefinitely. Set `HARNX_CLEANUP_REMOTE_SESSIONS_DAYS=<days>` (e.g. `7`) on the worker to enable hourly sweeps that delete sessions older than the retention threshold.
- **Operator inspection and deletion**: Operators can view existing sessions with `harnx list sessions` (backing sessions created by `harnx-mcp-server` appear with the agent identifier `<inline>`). Named agent sessions can be removed with `harnx delete session <session-id> --agent <agent> --cluster <cluster>`, while inline sessions are purged through worker retention GC once configured.
- **Tool reservation**: The server issues a worker tool reservation over NATS upon the first `list_tools` or `call_tool` request. This starts the required tool servers without acquiring the session execution lease.
- **Lifetime**: A background renewal loop keeps the reservation active. On connection teardown (stdio EOF, HTTP DELETE, inactivity timeout), pending calls are cancelled, the renewal loop stops, and the reservation is released.
- **Crash safety**: If a client crashes or the process is killed before clean release, worker-side TTL expiry automatically sweeps and reclaims the reservation.

For full implementation details on lifecycle guards and teardown sequencing, see [Transport lifecycle](docs/transports.md).

## Cluster Selection

Target a specific NATS cluster using `--cluster <name>`.

- **Precedence**: The `--cluster` CLI flag overrides the `HARNX_NATS_SERVER` environment variable.
- **Default resolution**:
  1. `--cluster <name>` if provided on the CLI.
  2. `HARNX_NATS_SERVER` if set (resolves to `nats_servers/<name>.yaml`).
  3. Local `__local__` cluster (embedded NATS broker and child worker) when neither is set.
- **Validation**: Named clusters must exist in `nats_servers/<name>.yaml`. Unknown names fail startup immediately:
  ```text
  unknown NATS cluster 'foo' (expected nats_servers/foo.yaml)
  ```
- **Single cluster**: Each server process connects to a single cluster. Multi-cluster proxying within a single process is not supported.

## HTTP Protocol Negotiation

`harnx-mcp-server` serves stateful MCP over Streamable HTTP at `/mcp`:

- **Protocol versions**: Supported versions are `2025-11-25`, `2025-06-18`, `2025-03-26`, and `2024-11-05`.
- **Automatic downgrade**: Standard rmcp clients request `2026-07-28` during initialization by default. The server automatically negotiates this down to `2025-11-25` during the initialize handshake. Clients do not need to manually pin their protocol version.
- **Sessionless / Discover clients rejected**: In rmcp 3.5.0, SEP-2567 removed stateful sessions from protocol `2026-07-28+`. Requests that skip `initialize` or attempt stateless Discover-only calls are rejected with an explicit error (`"initialize a stateful MCP HTTP session first"`). Stateful sessions are required to maintain backing session continuity and caller identity.
- **Inactivity timeout**: Default rmcp session inactivity timeout is 5 minutes. Sending an HTTP `DELETE` to `/mcp` terminates the transport session and releases its tool reservation immediately.

## Limitations

- **No HITL / confirmation hooks**: Tool confirmation is unsupported. Do not expose tools that require user confirmation; calls will defer or fail.
- **Direct tool calls**: Invoking tools directly through MCP bypasses agent tool-round hooks.
- **No resources or prompts**: MCP resources and prompts are not exported; only tools are exposed.
- **No handoff or history tools**: Dynamic handoff tools (`handoff_*`) and history tools (`session_history_*`) generated within agent prompt loops are excluded from the exported tool catalog.
