# harnx-a2a-server

`harnx-a2a-server` exposes explicitly selected harnx agents over the Agent2Agent (A2A 1.0) protocol using JSON-RPC and Server-Sent Events (SSE).

## Overview

Use `harnx-a2a-server` when you want external A2A clients—such as Atlassian Forge Jira remote agents (`rovo:agentConnector`), the `a2a-python` SDK, Google ADK, `a2a-inspector`, or LangGraph—to invoke harnx agents.

Each exported agent gets its own JSON-RPC endpoint and Agent Cards under `/agents/{name}`. Incoming requests create or resume durable, NATS-backed harnx sessions. The server handles task supervision, event streaming, deduplication, and session-scoped task persistence.

## CLI Flags and Options

```text
Usage: harnx-a2a-server [OPTIONS] --agent <SPEC>
```

| Flag | Default | Description |
|---|---|---|
| `--host <HOST>` | `127.0.0.1` | HTTP bind host. Set explicitly (for example, `0.0.0.0`) when listening behind a reverse proxy or container gateway. |
| `--port <PORT>` | `3020` | HTTP bind port. Distinct from toolset ports (3000–3007) and MCP HTTP (3010). |
| `--agent <SPEC>` | *(required)* | Agent export specification: bare `name` or `alias=name`. Repeatable, comma-separated. Environment variable `HARNX_A2A_AGENTS`. Specifying CLI flags replaces `HARNX_A2A_AGENTS`. There is no default "expose all" mode; at least one agent is required. |
| `--metrics-addr <ADDR>` | disabled | Optional Prometheus listener (`IP:PORT` or `:PORT`); exports recovery, pending-age and sweep diagnostics. |
| `--cluster <CLUSTER>` | none | Target NATS cluster name for shared workers. |
| `--config-dir <PATH>` | `HARNX_CONFIG_DIR` | Path to the harnx configuration directory containing `config.yaml`. |
| `--access-rules <PATH>` | `<config dir>/access.yaml` if present | Access rules file, also set by `HARNX_ACCESS_RULES`. Requires `--user-id-header` when rules are enabled. |
| `--public-base-url <URL>` | none | Base URL used in Agent Card interface URLs (for example, `https://agents.example.com`). If omitted, inferred from `X-Forwarded-*` or `Host` headers. |
| `--user-id-header <NAME>` | none | Trusted identity source: bare header name, `header:NAME`, or `cookie:NAME` (repeatable, first present source wins). Empty or invalid values fail closed. Enables user isolation mode. |
| `--group-header <NAME>` | none | Trusted group membership header name (repeatable, all values contribute). |
| `--role-header <NAME>` | none | Trusted role membership header name (repeatable, all values contribute). |
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

Agent Cards are public when access rules are disabled. With access rules enabled, cards require a trusted user identity and return HTTP 404 for hidden agents.

## Context and Task Semantics

- **Server-allocated contexts**: `contextId` is identical to the durable harnx session ID. Contexts are strictly allocated by the server. Incoming requests containing an unknown or unauthorized `contextId` are rejected with `TaskNotFoundError` (`-32001`). Clients cannot invent context IDs. With access rules enabled, `admin` can operate another user's context on the same export; creating a context still requires `prompt`.
- **Task ID format**: Task IDs are opaque strings formatted as `{contextId}.{uuid}`. Because harnx session IDs are base64url strings without dots, the dot cleanly delimits the context ID from the task UUID.
- **One active task per context**: A context can execute only one task at a time. If a client sends a new prompt to a context while a task is still running, the server rejects the request with code `-32000` ("context has an active task; retry later").
- **Message deduplication**: Requests are deduped by `messageId`.
  - Retrying an identical `messageId` and content returns the existing task record—even if the task has already reached a terminal state.
  - Sending an existing `messageId` with different content returns `InvalidParams` (`-32602`).
  - First-turn messages (without `contextId`) use a create-only NATS reservation scoped by `(cluster, export, user_id, messageId)`. Stable context/task/runtime IDs are retained before session creation or prompt append. Follow-up identities live in the CAS context authority before their session KV projection. Local gates and caches don't decide admission.
  - Initialization retries follow the same reservation. A bounded request can return an initialization error while its candidate lease is still held; retry with the same `messageId`, not a replacement. Owner-loss recovery closes a missing prompt instead of executing it. Reservations have no independent live-identity TTL.
- **Disconnections do not cancel**: Dropping an HTTP connection or closing an SSE stream does not cancel the turn. Execution continues on the worker. Clients can reconnect and resume streaming with `SubscribeToTask`.
- **Live output and persistence**: All replicas read committed snapshots and serve independent durable task streams. Coalesced artifact updates flush at 100 ms; each emitted update commits its full snapshot, cursor and one-event outbox before conditional JetStream publication. Streams start with a fresh snapshot, then contiguous updates. Lag, broker errors and retention gaps interrupt the reader; reconnect doesn't need a public cursor. Foreground/background recovery preserves durable completion or closes/stops the exact invocation without replay. Worker lease renewal doesn't block A2A recovery. No failover SLO is promised.
- **Durable task storage and session retention**: Task records, persisted Jira payloads, and deduplication entries live in NATS KV under the session's storage key prefix (`sessions/{storage_key}/a2a/...`). Those session-prefix records live as long as the backing session does. Global first-message reservations (`a2a/first-messages/{scope-hash}`) and recovery registrations (`a2a.registry.{storage-hash}`) have no independent TTL; coordinated session GC removes them, the task event subject and the scoped lease only after durable task settlement. Unresolved work blocks deletion. Remote session GC runs only when the worker setting `cleanup_remote_sessions_days` (environment variable `HARNX_CLEANUP_REMOTE_SESSIONS_DAYS`) is positive; by default, it is unset (`None`), meaning automatic GC is disabled and sessions persist indefinitely. Set `cleanup_remote_sessions_days: <days>` (or `HARNX_CLEANUP_REMOTE_SESSIONS_DAYS=<days>`) in worker configuration to enable hourly cleanup sweeps. Operators can also delete a session explicitly with `harnx delete session <session-id> --agent <agent> --cluster <cluster>`.

## Task index and upgrades

Task metadata lives at `sessions/{storage_key}/a2a/index`. New contexts get an
empty index when bound. First access to a legacy session without that key scans
the shared bucket once and builds the index from legacy task records. An
existing empty index does not trigger a scan.

Only one server version may serve a bucket at a time. Stop the old server
before starting the new one; don't overlap versions during rolling updates.
See [replica operations](../../docs/a2a-operations.md) for provisioning, calibrated payload limits, retention, permissions and drain rollout.
After running a pre-index build during a downgrade, delete each affected
session's `sessions/{storage_key}/a2a/index` key before re-upgrading. Keep the
task records; first access will rebuild the index.

The index is one KV value and retains all terminal tasks. Its size is capped
by the bucket's maximum value size and the broker's maximum payload. With a
1 MiB limit, expect a few thousand tasks per session (IDs and timestamps
change the exact count). A create that would exceed the limit fails; server
logs report `task index exceeds maximum size limit`. Clients receive the
pre-admission error `-32603 "request failed"`. Start a new context instead.
Missing task records leave creation intents in the index. Intents still
missing after five minutes are removed on listing or reconciliation.

Coordinated admission writes active identity in the context authority before index
repair. Index lag or a failed repair cannot hide an active task from admission.
Legacy create-only task writes keep their index-first ordering. Recovery uses the
original runtime ticket: durable completion wins over owner-loss failure; a missing
prompt is closed, never replayed. Terminal archives and the terminal event outbox
must be durable before the context accepts another task.

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
| `-32001` | `TASK_NOT_FOUND` | Task or server-allocated context does not exist, or the caller can't access its agent, owner or export. |
| `-32002` | `TASK_NOT_CANCELABLE` | Task is already in a terminal state (`COMPLETED`, `FAILED`, `CANCELED`). |
| `-32003` | `PUSH_NOT_SUPPORTED` | Push notification configuration requested (push notifications unsupported). |
| `-32004` | `UNSUPPORTED_OPERATION` | `SubscribeToTask` called on a task that is already terminal, or extended cards requested. |
| `-32005` | `CONTENT_TYPE_NOT_SUPPORTED` | Unsupported raw file media type in message part. |
| `-32009` | `VERSION_NOT_SUPPORTED` | Unsupported explicit `A2A-Version` header. |
| `-32010` | Server-specific permission denial | Access rules are enabled and creating a context requires `prompt` scope (`session creation requires prompt scope`). |
| `-32600` | `INVALID_REQUEST` | Malformed JSON-RPC envelope or invalid parameters shape. |
| `-32601` | `METHOD_NOT_FOUND` | Unrecognized JSON-RPC method. |
| `-32602` | `INVALID_PARAMS` | Validation failure: oversized data part, missing required fields, `ListTasks` missing `contextId`, or duplicate `messageId` with different content. |
| `-32603` | `INTERNAL_ERROR` | Internal server or worker execution error. |
| `-32700` | `PARSE_ERROR` | Request body is not valid JSON. |


## Access Control (`access.yaml`)

`harnx-a2a-server` supports optional identity-based access control rules gating agent exports, Agent Cards, and task/context lifecycles. See the [Access Control section in the Configuration Guide](../../docs/configuration-guide.md#access-control-accessyaml) for full configuration details.

### Enabling Rules
- Pass `--access-rules <PATH>` on the CLI or set `HARNX_ACCESS_RULES`. If unspecified, the server automatically checks `<config-dir>/access.yaml`.
- When access rules are enabled, `--user-id-header` is **required**. Startup fails closed if rules are active without configured identity headers.
- When no rules file or flag is present, access control is disabled and standard A2A behavior applies.

### Behavior Under Rules
- **Authentication**: All endpoints under `/agents/{name}`, including Agent Cards and discovery GET requests, require trusted caller identity and return HTTP `401 Unauthorized` (JSON-RPC error code `-32000`) if missing. (When access rules are disabled, Agent Cards remain public).
- **Agent Reference Matching**: Rules are evaluated against the internal agent reference (`Export::agent_ref()`, such as `coder` or `coder@cluster`), **never** against public export aliases (such as `alias=coder`).
- **Hidden Exports**: Exports for which the caller has no scope return HTTP `404 Not Found` for HTTP discovery and JSON-RPC `-32001` (`task not found`) for RPC requests, preventing enumeration.
- **Context and Task Permissions**:
  - `prompt`: Authorizes creating new contexts/tasks and continuing own contexts.
  - `admin`: Authorizes listing, reading, and continuing tasks in existing contexts across all users (and legacy unowned contexts). Does not grant creation permission.
  - Context creation without `prompt` scope returns JSON-RPC permission error code `-32010` (`session creation requires prompt scope`).
  - Accessing another user's context without `admin` scope returns JSON-RPC `-32001` (`task not found`).
- **ListTasks Scoping**: `ListTasks` requires `contextId`. Callers with `prompt` scope can list tasks in their own contexts; callers with `admin` scope can list tasks in any authorized context.
- **Group and Role Memberships**: Callers can supply group and role memberships via `--group-header <NAME>` and `--role-header <NAME>` flags. All configured headers contribute values; repeated headers and comma-separated tokens are split and trimmed. Any invalid header bytes fail closed with JSON-RPC error code `-32000` (HTTP 401). Memberships are evaluated per request and never persisted in session metadata or NATS task storage. Memberships never satisfy context ownership (which remains tied solely to caller `user_id`), but rules granting `admin` scope allow full task management across all contexts.
- **User Aliases (`users.yaml`)**: Optional user alias configuration loaded once at startup from `<config-dir>/users.yaml`. See the [User Aliases section in the Configuration Guide](../../docs/configuration-guide.md#user-aliases-usersyaml) for schema, semantics, and examples.
  - An authenticated caller's identity expands into the first matching entry's `identities` list for agent export visibility and context access.
  - Stored owner identities in A2A bindings are never expanded. New contexts record the caller's raw incoming identity in the binding.
  - When access rules are disabled, owner isolation still applies: callers can access bindings where the stored owner is in their expanded identities. Anonymous access (`None` stored owner and `None` caller) succeeds; mixed anonymous/authenticated access fails.
  - Missing `users.yaml` preserves singleton identity behavior. An invalid present file fails startup immediately with path context. Modifying the file requires a server restart; there is no live reload.
  - Because alias mappings grant authorization, `users.yaml` should be edited only by trusted operators.

## Deploying Behind a Reverse Proxy

In production, run `harnx-a2a-server` behind a reverse proxy (such as Nginx, Envoy, or Cloudflare).

### Reverse Proxy Responsibilities

1. **Authentication and JWT verification**: The server does not validate tokens or authentication signatures. For Atlassian Forge apps, the reverse proxy must verify the Forge Invocation Token (FIT JWT) signed by Atlassian's JWKS.
2. **Strip or overwrite identity sources**: When `--user-id-header`, `--group-header`, or `--role-header` are configured, the proxy **must strip or overwrite** the selected headers or cookies on incoming requests from clients. Cookies must contain a proxy-verified user ID, not a token or client-supplied identity. The server doesn't verify cookie signatures.
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
- **Multi-replica routing**: Shared admission, ownership, cancellation and streaming work through any backend without sticky sessions. Drain old turns and exclude mixed writers when upgrading; deploy runtime readers before `admission_closed` writers. See [operations](../../docs/a2a-operations.md). Recovery is eventual and TTL-based, not a fixed failover SLO. Interrupted work is never replayed and tool side effects aren't rolled back.
- **No push notifications**: Push notification methods (`CreateTaskPushNotificationConfig`, etc.) return `PushNotificationNotSupportedError` (`-32003`). `capabilities.pushNotifications` is set to `false`.
- **Anonymous mode lacks isolation**: When `--user-id-header` is not configured, all requests operate as a single anonymous principal. Any client reaching the endpoint can access or resume sessions.
- **A2A 0.3 compatibility is input-only**: The server accepts 0.3 method names, legacy `blocking`, and relaxed enum names on incoming requests, but always responds with canonical A2A 1.0 wire payloads. Full 0.3 wire responses are not supported.
- **ListTasks requires contextId**: Unscoped listing across all tasks is disabled. `ListTasks` requires a non-empty `contextId` and lists only tasks within that authorized session.
- **A2A TCK waivers**: The TCK workflow is gating, with 16 known failures listed individually in [`scripts/a2a-tck/waivers.toml`](../../scripts/a2a-tck/waivers.toml). Strict pytest xfail markers keep these tests running and report their reasons. Any unwaived failure, unexpected pass (`XPASS(strict)`), or waiver matching no collected test fails the job. When a test is fixed, remove its `[[waiver]]` entry and rerun `scripts/run-a2a-tck.sh`; see the [waiver guide](../../scripts/a2a-tck/README.md) for categories and scope.
- **protoc required at build time**: The underlying `a2a-pb` crate compiles protocol buffer schemas during the build. While release builds and CI can use vendored binaries, environments with non-executable cargo home directories require a system compiler via `PROTOC=/usr/bin/protoc`.
