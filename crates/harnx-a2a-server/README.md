# harnx-a2a-server

`harnx-a2a-server` exposes explicitly selected harnx agents over the Agent2Agent (A2A 1.0) protocol using JSON-RPC and Server-Sent Events (SSE).

## Overview

Use `harnx-a2a-server` when you want external A2A clients—such as Atlassian Forge Jira remote agents (`rovo:agentConnector`), the `a2a-python` SDK, Google ADK, `a2a-inspector`, or LangGraph—to invoke harnx agents.

Each exported agent gets its own JSON-RPC endpoint and public Agent Cards under `/agents/{name}`. Incoming requests create or resume durable, NATS-backed harnx sessions. The server handles task supervision, event streaming, deduplication, and session-scoped task persistence.

## CLI Flags and Options

```text
Usage: harnx-a2a-server [OPTIONS] --agent <SPEC>
```

| Flag | Default | Description |
|---|---|---|
| `--host <HOST>` | `127.0.0.1` | HTTP bind host. Set explicitly (for example, `0.0.0.0`) when listening behind a reverse proxy or container gateway. |
| `--port <PORT>` | `3020` | HTTP bind port. Distinct from toolset ports (3000–3007) and MCP HTTP (3010). |
| `--agent <SPEC>` | *(required)* | Agent export specification: bare `name` or `alias=name`. Repeatable, comma-separated. Environment variable `HARNX_A2A_AGENTS`. Specifying CLI flags replaces `HARNX_A2A_AGENTS`. There is no default "expose all" mode; at least one agent is required. |
| `--cluster <CLUSTER>` | none | Target NATS cluster name for shared workers. |
| `--config-dir <PATH>` | `HARNX_CONFIG_DIR` | Path to the harnx configuration directory containing `config.yaml`. |
| `--public-base-url <URL>` | none | Base URL used in Agent Card interface URLs (for example, `https://agents.example.com`). If omitted, inferred from `X-Forwarded-*` or `Host` headers. |
| `--user-id-header <NAME>` | none | Trusted identity source: bare header name, `header:NAME`, or `cookie:NAME` (repeatable, first present source wins). Empty or invalid values fail closed. Enables user isolation mode. |
| `--max-data-part-bytes <BYTES>` | `65536` | Maximum combined byte budget for rendered data and inline text/JSON file parts per message. Over-limit requests return an invalid params error. |

### Export Specifications and Startup Validation

Pass agent exports using `--agent <SPEC>`. For example:

```sh
harnx-a2a-server \
  --agent reviewer \
  --agent jira=team/triage \
  --user-id-header X-User-Id
```

- **Explicit exports only**: Wildcards (`*`, `?`, glob characters) are rejected.
- **Agent validation**: Every exported agent must exist in local agent markdown or builtin configuration at startup. Unknown agents cause startup failure.
- **Lookup key uniqueness**: Any collision across export lookup keys fails startup immediately.

## Endpoints and Name Resolution

For each exported agent, the server exposes endpoints rooted under `/agents/{name}`:

| Method | Path | Description |
|---|---|---|
| `GET` | `/agents/{name}/.well-known/agent-card.json` | A2A 1.0 Agent Card |
| `GET` | `/agents/{name}/.well-known/agent.json` | Legacy 0.3 Agent Card alias (returns identical 1.0 card payload) |
| `POST` | `/agents/{name}` or `/agents/{name}/` | A2A JSON-RPC 2.0 endpoint (unary and SSE streaming) |

### Name Forms

An agent can be addressed by three URL segment forms:

1. **Explicit alias**: The alias given on the CLI (for example, `--agent jira=team/triage` exposes `/agents/jira`).
2. **Sanitized package form (`pkg__agent`)**: Slashes in package-qualified names are sanitized to double underscores (for example, `team/triage` is exposed as `/agents/team__triage`).
3. **Percent-encoded form (`pkg%2Fagent`)**: Decoded by the router (for example, `/agents/team%2Ftriage`).

**Public URLs never contain `%2F`**: In generated Agent Cards, `supportedInterfaces[].url` always uses the explicit alias if configured, or the sanitized `pkg__agent` form. Reverse proxies never receive `%2F` from public cards.

Agent Cards are public discovery documents and never require authentication or user identity headers.

## Context and Task Semantics

- **Server-allocated contexts**: `contextId` is identical to the durable harnx session ID. Contexts are strictly allocated by the server. Incoming requests containing an unknown or foreign `contextId` are rejected with `TaskNotFoundError` (`-32001`). Clients cannot invent context IDs.
- **Task ID format**: Task IDs are opaque strings formatted as `{contextId}.{uuid}`. Because harnx session IDs are base64url strings without dots, the dot cleanly delimits the context ID from the task UUID.
- **One active task per context**: A context can execute only one task at a time. If a client sends a new prompt to a context while a task is still running, the server rejects the request with code `-32000` ("context has an active task; retry later").
- **Message deduplication**: Requests are deduped by `messageId`.
  - Retrying an identical `messageId` and content returns the existing task record—even if the task has already reached a terminal state.
  - Sending an existing `messageId` with different content returns `InvalidParams` (`-32602`).
  - First-turn messages (without `contextId`) are deduped through an in-memory LRU cache keyed by `(export, user_id, messageId)`. Follow-up messages within a context are deduped against session KV storage.
- **Disconnections do not cancel**: Dropping an HTTP connection or closing an SSE stream does not cancel the turn. Execution continues on the worker. Clients can reconnect and resume streaming with `SubscribeToTask`.
- **Durable task storage and session retention**: Task records, persisted Jira payloads, and deduplication entries live in NATS KV under the session's storage key prefix (`sessions/{storage_key}/a2a/...`). They live as long as the backing session does. Remote session GC runs only when the worker setting `cleanup_remote_sessions_days` (environment variable `HARNX_CLEANUP_REMOTE_SESSIONS_DAYS`) is positive; by default, it is unset (`None`), meaning automatic GC is disabled and sessions persist indefinitely. Set `cleanup_remote_sessions_days: <days>` (or `HARNX_CLEANUP_REMOTE_SESSIONS_DAYS=<days>`) in worker configuration to enable hourly cleanup sweeps. Operators can also delete a session explicitly with `harnx delete session <session-id> --agent <agent> --cluster <cluster>`.

## Data Parts and Rendering

Harnx executes turns using prompt text and runtime attachments. Inbound A2A parts are rendered into the prompt in original order, joined by blank lines:

- **Text parts**: Inlined verbatim.
- **Data parts**: Pretty-printed JSON (2-space indentation) wrapped in a labeled fence:
  ```text
  --- A2A data part (mediaType: application/json) ---
  ```json
  {
    "userAccountId": "557058:...",
    "issue": {
      "key": "PROJ-123"
    }
  }
  ```
  --- End A2A data part ---
  ```
  If `filename` is provided on the part, it is included in the header banner.
- **Inline raw file parts**:
  - `image/*`: Converted to data URLs and passed to the agent as multimodal media inputs.
  - `text/*` and `application/json`: UTF-8 decoded and wrapped in the data banner format.
  - Other media types return `ContentTypeNotSupportedError` (`-32005`).
- **File URLs**: Rendered as descriptive reference tags: `[A2A file: {filename} ({mediaType}) {url}]`. The server does not fetch URLs; the agent decides whether to fetch them with its own tools.
- **Size limit (`--max-data-part-bytes`)**: Defaults to 64 KiB (`65536` bytes). The budget covers rendered data blocks and inline text/JSON file blocks (including banners and code fences). Exceeding this limit returns `InvalidParams` (`-32602`). Content is never silently truncated.
- **Task history preservation**: The original inbound parts are saved verbatim in task history. `GetTask` returns the unrendered input parts sent by the client.

## Protocol Version Policy and 0.3 Compatibility

- **A2A-Version header**: The server accepts `1.0`, numeric patch formats like `1.0.1`, explicit `0.3`, and missing headers. A missing `A2A-Version` header defaults to `1.0` (with a debug log). Any other explicit version returns `VersionNotSupported` (`-32009`).
- **Input compatibility only**: Legacy 0.3 method names and enum formats are accepted on input and translated before dispatch:
  - `message/send` → `SendMessage`
  - `message/stream` → `SendStreamingMessage`
  - `tasks/get` → `GetTask`
  - `tasks/cancel` → `CancelTask`
  - `tasks/resubscribe` → `SubscribeToTask`
  - Legacy `blocking` boolean is translated to `returnImmediately` (conflicting explicit values return `-32602`).
  - Lenient enum parsing accepts lowercase or unprefixed state and role values (for example, `working` → `TASK_STATE_WORKING`, `user` → `ROLE_USER`).
- **1.0 wire responses**: The server always emits canonical A2A 1.0 JSON-RPC responses, ProtoJSON task states, and event envelopes. Full 0.3 wire responses are not produced.

## Error Codes

The server emits JSON-RPC 2.0 error responses with structured `google.rpc.ErrorInfo` details:

| Code | Reason | Description |
|---|---|---|
| `-32000` | Server-specific | Missing or empty user identity header (HTTP 401), or busy context (active task in progress). |
| `-32001` | `TASK_NOT_FOUND` | Task or server-allocated context does not exist, or belongs to another user/export. |
| `-32002` | `TASK_NOT_CANCELABLE` | Task is already in a terminal state (`COMPLETED`, `FAILED`, `CANCELED`). |
| `-32003` | `PUSH_NOT_SUPPORTED` | Push notification configuration requested (push notifications unsupported). |
| `-32004` | `UNSUPPORTED_OPERATION` | `SubscribeToTask` called on a task that is already terminal, or extended cards requested. |
| `-32005` | `CONTENT_TYPE_NOT_SUPPORTED` | Unsupported raw file media type in message part. |
| `-32009` | `VERSION_NOT_SUPPORTED` | Unsupported explicit `A2A-Version` header. |
| `-32600` | `INVALID_REQUEST` | Malformed JSON-RPC envelope or invalid parameters shape. |
| `-32601` | `METHOD_NOT_FOUND` | Unrecognized JSON-RPC method. |
| `-32602` | `INVALID_PARAMS` | Validation failure: oversized data part, missing required fields, `ListTasks` missing `contextId`, or duplicate `messageId` with different content. |
| `-32603` | `INTERNAL_ERROR` | Internal server or worker execution error. |
| `-32700` | `PARSE_ERROR` | Request body is not valid JSON. |

## Deploying Behind a Reverse Proxy

In production, run `harnx-a2a-server` behind a reverse proxy (such as Nginx, Envoy, or Cloudflare).

### Reverse Proxy Responsibilities

1. **Authentication and JWT verification**: The server does not validate tokens or authentication signatures. For Atlassian Forge apps, the reverse proxy must verify the Forge Invocation Token (FIT JWT) signed by Atlassian's JWKS.
2. **Strip or overwrite identity sources**: When `--user-id-header` is configured, the proxy **must strip or overwrite** the selected header or cookie on incoming requests from clients. Cookies must contain a proxy-verified user ID, not a token or client-supplied identity. The server doesn't verify cookie signatures.
3. **Disable SSE response buffering**: Streaming responses use Server-Sent Events (`text/event-stream`). The proxy must disable buffer accumulation (e.g. `proxy_buffering off` in Nginx; `X-Accel-Buffering: no` is emitted by the server).
4. **Long route timeouts**: Remote agent turns can run for several minutes. Set proxy read and send timeouts to at least **900 seconds** (15 minutes), matching Forge's SSE stream allowance.
5. **Base URL configuration**: Set `--public-base-url https://agents.example.com` or pass standard forwarding headers (`Host`, `X-Forwarded-Proto`, `X-Forwarded-Host`) so Agent Cards generate reachable public URLs. Set `--public-base-url` in production so the card origin does not depend on request headers.

### Nginx Configuration Example

```nginx
upstream harnx_a2a {
    server 127.0.0.1:3020;
    keepalive 32;
}

server {
    listen 443 ssl http2;
    server_name agents.example.com;

    # SSL configuration omitted for brevity...

    location / {
        # 1. Reverse proxy to harnx-a2a-server
        proxy_pass http://harnx_a2a;
        proxy_http_version 1.1;

        # 2. Forwarded base URL headers (explicitly overwrite client values)
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;

        # 3. User identity header handling:
        # Strip any client-supplied header and inject the verified identity from auth
        proxy_set_header X-User-Id $authenticated_user_id;

        # 4. SSE streaming settings
        proxy_buffering off;
        proxy_cache off;
        proxy_set_header Connection '';
        chunked_transfer_encoding off;

        # 5. Route timeouts >= 900 seconds
        proxy_connect_timeout 60s;
        proxy_send_timeout 900s;
        proxy_read_timeout 900s;
    }
}
```

## Atlassian Forge Manifest Example

To connect Jira Remote Agents via Forge, define a `rovo:agentConnector` module in your Forge `manifest.yml`. References:
- [Rovo Agent Connector module reference](https://developer.atlassian.com/platform/forge/manifest-reference/modules/rovo-agent-connector/)
- [Integrate remote agents with Jira](https://developer.atlassian.com/platform/forge/remote-agents-in-jira/)

```yaml
modules:
  rovo:agentConnector:
    - key: harnx-jira-agent
      name: Harnx Jira Assistant
      description: Autonomous agent powered by harnx
      productContexts:
        - jira
      protocols:
        agent2Agent:
          version: '1.0'
          jsonRpcTransport:
            endpoint: a2a-endpoint
            streaming: true

  endpoint:
    - key: a2a-endpoint
      remote: harnx-agent-remote
      route: /

remotes:
  - key: harnx-agent-remote
    baseUrl: https://a2a.example.com/agents/jira
    operations:
      - compute
```

## Limitations

- **No HITL confirmations**: A2A 1.0 does not specify an interactive human-in-the-loop confirmation flow. Any tool execution that requires manual approval is automatically denied (fails closed).
- **Single replica in v1**: Active turn execution and runner registries are in-memory. Multi-replica routing and distributed execution ownership are not supported in this release.
- **No push notifications**: Push notification methods (`CreateTaskPushNotificationConfig`, etc.) return `PushNotificationNotSupportedError` (`-32003`). `capabilities.pushNotifications` is set to `false`.
- **Anonymous mode lacks isolation**: When `--user-id-header` is not configured, all requests operate as a single anonymous principal. Any client reaching the endpoint can access or resume sessions.
- **A2A 0.3 compatibility is input-only**: The server accepts 0.3 method names, legacy `blocking`, and relaxed enum names on incoming requests, but always responds with canonical A2A 1.0 wire payloads. Full 0.3 wire responses are not supported.
- **ListTasks requires contextId**: Unscoped listing across all tasks is disabled. `ListTasks` requires a non-empty `contextId` and lists only tasks within that authorized session.
- **A2A TCK waivers**: The TCK workflow is gating, with 16 known failures listed individually in [`scripts/a2a-tck/waivers.toml`](../../scripts/a2a-tck/waivers.toml). Strict pytest xfail markers keep these tests running and report their reasons. Any unwaived failure, unexpected pass (`XPASS(strict)`), or waiver matching no collected test fails the job. When a test is fixed, remove its `[[waiver]]` entry and rerun `scripts/run-a2a-tck.sh`; see the [waiver guide](../../scripts/a2a-tck/README.md) for categories and scope.
- **protoc required at build time**: The underlying `a2a-pb` crate compiles protocol buffer schemas during the build. While release builds and CI can use vendored binaries, environments with non-executable cargo home directories require a system compiler via `PROTOC=/usr/bin/protoc`.
