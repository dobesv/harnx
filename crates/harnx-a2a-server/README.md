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
| `--healthz-addr <ADDR>` | disabled | Optional readiness listener at `/healthz` (`IP:PORT` or `:PORT`); environment variable `HARNX_HEALTHZ_ADDR`. CLI takes precedence. |
| `--metrics-addr <ADDR>` | disabled | Optional Prometheus listener at `/metrics` (`IP:PORT` or `:PORT`); exports HTTP request metrics and recovery, pending-age and sweep diagnostics. CLI only, no environment variable fallback. |
| `--cluster <CLUSTER>` | none | Target NATS cluster name for shared workers. |
| `--config-dir <PATH>` | `HARNX_CONFIG_DIR` | Path to the harnx configuration directory containing `config.yaml`. |
| `--access-rules <PATH>` | `<config dir>/access.yaml` if present | Access rules file, also set by `HARNX_ACCESS_RULES`. Requires `--user-id-header` when rules are enabled. |
| `--public-base-url <URL>` | none | Base URL used in Agent Card interface URLs (for example, `https://agents.example.com`). If omitted, inferred from `X-Forwarded-*` or `Host` headers. |
| `--user-id-header <NAME>` | none | Trusted identity source: bare header name, `header:NAME`, `cookie:NAME`, `forge-fit`, or `bearer-claim:CLAIM` (repeatable, first present source wins). Bare `forge-fit` is the Forge token source, not a header name. Without verification flags, bearer sources operate in decode-only mode. Empty or invalid values fail closed. Enables user isolation mode. |
| `--group-header <NAME>` | none | Trusted group membership header name (repeatable, all values contribute). |
| `--role-header <NAME>` | none | Trusted role membership header name (repeatable, all values contribute). |
| `--bearer-jwks-url <URL>` | none | Verify bearer identity sources against this JWKS URL. Requires `--bearer-issuer` and `--bearer-audience`; off by default. |
| `--bearer-issuer <ISSUER>` | none | Expected bearer token issuer (requires `--bearer-jwks-url` and `--bearer-audience`). |
| `--bearer-audience <AUDIENCE>` | none | Expected bearer token audience (requires `--bearer-jwks-url` and `--bearer-issuer`). Pinning audience is essential because all Forge apps share signing keys. |
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

## Readiness and metrics ports

```sh
harnx-a2a-server --agent reviewer --host 0.0.0.0 --port 3020 \
  --healthz-addr :8081 --metrics-addr :8456
curl -i http://127.0.0.1:8081/healthz
curl http://127.0.0.1:8456/metrics
```

A2A traffic uses port `3020` by default. Readiness and metrics are opt-in,
independent listeners; `8081` and `8456` are example ports, not defaults. Don't
reuse the application port or put both optional listeners on the same port.
Blank hosts bind `0.0.0.0`. Restrict readiness and metrics to your probe/scraper
network; they don't use A2A authentication or access rules.

`GET /healthz` returns an empty `503 Service Unavailable` until agent validation,
NATS/JetStream connection, coordination provisioning, backend initialization and
the A2A listener bind succeed, then an empty `200 OK`. SIGTERM or Ctrl-C flips it
back to `503` before runners are aborted/drained and HTTP shutdown finishes. This
is startup/shutdown readiness, not a continuous check of broker or worker health.
Use it for a Kubernetes readiness probe, not a liveness probe.

`GET /metrics` serves Prometheus text, including HTTP request counts and durations
with bounded route-template labels, plus A2A recovery diagnostics. Metrics use
`--metrics-addr` only, matching `harnx-serve` and `harnx-worker`; they don't read
`HARNX_METRICS_ADDR`. See [operations](../../docs/a2a-operations.md#listeners-and-readiness)
and [Kubernetes deployment](../../docs/kubernetes-deployment.md#a2a-backends-behind-ingress).

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

### Caller Identity Sources

Configure caller identity sources with `--user-id-header <SPEC>`. The flag is repeatable; sources are evaluated in order and the first present source wins.

| Source Syntax | Wire Location | Resolved Identity | Description |
|---|---|---|---|
| `NAME` or `header:NAME` | Header `NAME` | Header value | Reads first comma-separated token, trimmed. Fails closed on empty values. |
| `cookie:NAME` | Cookie `NAME` | Cookie value | Cookie names are case-sensitive. Must carry a proxy-verified user ID, not a token. |
| `forge-fit` | `Authorization: Bearer <FIT>` | X.500 DN | Base64url-decodes Atlassian Forge Invocation Token payload and formats a DN. Bare `forge-fit` is the source name, not a header name. |
| `bearer-claim:CLAIM` | `Authorization: Bearer <JWT>` | Claim value | Reads named top-level string claim from the decoded token payload. |

#### Forge DN Format

The `forge-fit` source formats caller identity into an X.500 Distinguished Name (DN) ordered CN-first:

- **User invocations** (`principal` claim present and not `"unidentified"`):
  `CN=<principal>,OU=<context.cloudId>,O=atlassian.com`
  Example: `CN=655362:312d3308-8954-42b0-aa38-771a10c88656,OU=d0d52620-3203-4cfa-8db5-f2587155f0dd,O=atlassian.com`
- **Automation and anonymous invocations** (`principal` claim absent or `"unidentified"`):
  `OU=<context.cloudId>,O=atlassian.com`
  Example: `OU=d0d52620-3203-4cfa-8db5-f2587155f0dd,O=atlassian.com`

Component values are validated against control characters, leading/trailing whitespace, and DN delimiter characters (`,`, `=`, `+`, `<`, `>`, `#`, `;`, `"`, `\`). Any invalid character fails closed (HTTP 401 / JSON-RPC `-32000`).

#### Shared-Owner Caveat for Org-Only Identities

All calls lacking a user principal from the same Atlassian site/cloudId (system automations, scheduled triggers, lifecycle events, anonymous callers) resolve to the same org-only identity `OU=<cloudId>,O=atlassian.com`. Because A2A session ownership is keyed to the exact resolved identity string, these automated tasks share a single owner: any task or trigger from that site can view, prompt, or manage other sessions owned by that org-only identity under `prompt` scope.

#### Fallthrough and Fail-Closed Precedence

- Sources are evaluated in `--user-id-header` order. The first source present in the request wins.
- If `Authorization` is absent or uses a scheme other than `Bearer`, the bearer source is skipped and resolution falls through to the next configured source.
- If a bearer source is selected and an `Authorization: Bearer` token is present, resolution never falls through. If the token is malformed, missing required claims, expired, or fails verification, the request fails closed immediately with HTTP 401 / JSON-RPC `-32000`. Later sources are never tried.
- Earlier sources take precedence. For example, with `--user-id-header header:X-User-Id --user-id-header forge-fit`, a valid `X-User-Id` header takes precedence even if an invalid bearer token is also sent.
- If no configured source matches (the end of the source list is reached), the request is rejected with HTTP 401 / JSON-RPC `-32000`. When identity sources are configured, this rejection protects all endpoints under `/agents/{name}`, including Agent Cards when access rules are enabled (when access rules are disabled, Agent Cards remain public).
- With zero sources configured (no `--user-id-header` flags), the server runs in shared anonymous mode and accepts requests without identity checks.
- There is no separate "require a token" flag. The verification flags do not make a token mandatory by themselves; they ensure any present token is verified. A token becomes mandatory when the bearer source is the only configured `--user-id-header` source, because a request omitting the token reaches the end of the source list and is rejected with HTTP 401 (see [Built-in Bearer Token Verification](#built-in-bearer-token-verification)).

#### Access Control and Alias Examples for Forge DNs

In `access.yaml`:

```yaml
rules:
  # Allow all authenticated Jira users from a specific site
  - agents:
      - triage-agent
    scopes:
      - prompt
    users:
      - "CN=*,OU=d0d52620-3203-4cfa-8db5-f2587155f0dd,O=atlassian.com"

  # Match both user calls and automated/no-user calls from the same site
  # (*OU matches CN=...,OU=... and OU=... because * spans commas)
  - agents:
      - triage-agent
    scopes:
      - prompt
    users:
      - "*OU=d0d52620-3203-4cfa-8db5-f2587155f0dd,O=atlassian.com"
```

In `users.yaml`, map a specific Forge DN to display metadata, alias groups, and roles:

```yaml
- name: Forge Jira Lead
  identities:
    - "CN=655362:312d3308-8954-42b0-aa38-771a10c88656,OU=d0d52620-3203-4cfa-8db5-f2587155f0dd,O=atlassian.com"
  groups:
    - engineering
  roles:
    - admin
```

#### Finding Cloud ID and Account ID

To configure allow lists in `access.yaml` or user mappings in `users.yaml`, operators need the site's Cloud ID (`OU`) and the user's Account ID (`CN`):

- **Cloud ID (`OU`)**: Identifies the Atlassian site (UUID format).
  - Query `https://<site>.atlassian.net/_edge/tenant_info` (an unauthenticated endpoint returning JSON with `cloudId`).
  - Read `context.cloudId` from a Forge invocation payload or app logs.
  - Check the site's settings in Atlassian Administration (`admin.atlassian.com`).
- **Account ID (`CN`)**: Identifies the user within Atlassian (e.g. `655362:312d3308-...`).
  - Open the user's profile in Jira; the URL is `https://<site>.atlassian.net/jira/people/<accountId>`.
  - Call the Jira REST API `GET /rest/api/3/myself`, which returns the caller's `accountId`.
  - Look up the user in Atlassian Administration (`admin.atlassian.com`) under Directory > Users.

Atlassian account IDs are global across sites, while `OU` pins the specific site. Combining them as `CN=<id>,OU=<cloudId>,O=atlassian.com` restricts the user's access to that single site.

For security and privacy, `harnx-a2a-server` never logs resolved caller identities or token claims in server logs. After a test call, operators can inspect the resolved identity in session metadata: `harnx info session <agent> <session-id> --format json` records the identity under `extensions["dev.harnx.session_properties"].user_id.value`.

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
  - Optional `groups` and `roles` declared on the matched entry are merged into caller memberships (appended after header values and deduped) and matched against `access.yaml` rules, even without membership headers configured.
  - Stored owner identities in A2A bindings are never expanded. New contexts record the caller's raw incoming identity in the binding. Configured groups and roles never grant context ownership and are not stored in task bindings or session metadata.
  - When access rules are disabled, owner isolation still applies: callers can access bindings where the stored owner is in their expanded identities. Anonymous access (`None` stored owner and `None` caller) succeeds; mixed anonymous/authenticated access fails.
  - Missing `users.yaml` preserves singleton identity behavior. An invalid present file fails startup immediately with path context. Modifying the file requires a server restart; there is no live reload.
  - Because alias mappings grant authorization, `users.yaml` should be edited only by trusted operators.

## Deploying Behind a Reverse Proxy

In production, run `harnx-a2a-server` behind a reverse proxy (such as Nginx, Envoy, or Cloudflare).

### Reverse Proxy Responsibilities

1. **Authentication and token verification**: For bearer tokens, built-in verification (`--bearer-jwks-url`, `--bearer-issuer`, `--bearer-audience`) is recommended. If you verify tokens at the reverse proxy instead, configure bearer sources (`forge-fit`, `bearer-claim:CLAIM`) without verification flags; the server will operate in decode-only mode and trust the upstream proxy's validation.
2. **Strip or overwrite identity sources**: When `--user-id-header`, `--group-header`, or `--role-header` are configured, the proxy **must strip or overwrite** the selected headers or cookies on incoming requests from clients. Cookies must contain a proxy-verified user ID, not a token or client-supplied identity. The server doesn't verify cookie signatures.
3. **Disable SSE response buffering**: Streaming responses use Server-Sent Events (`text/event-stream`). The proxy must disable buffer accumulation (e.g. `proxy_buffering off` in Nginx; `X-Accel-Buffering: no` is emitted by the server).
4. **Long route timeouts**: Remote agent turns can run for several minutes. Set proxy read and send timeouts to at least **900 seconds** (15 minutes), matching Forge's SSE stream allowance.
5. **Base URL configuration**: Set `--public-base-url https://agents.example.com` or pass standard forwarding headers (`Host`, `X-Forwarded-Proto`, `X-Forwarded-Host`) so Agent Cards generate reachable public URLs. Set `--public-base-url` in production so the card origin does not depend on request headers.

### Trust Model

Bearer token configuration has two distinct parts:
1. **Identity extraction**: Bearer identity sources (`forge-fit` and `bearer-claim:CLAIM`, configured via `--user-id-header`) read caller identity from the `Authorization: Bearer` token.
2. **Token verification**: Cryptographic verification flags (`--bearer-jwks-url`, `--bearer-issuer`, `--bearer-audience`) validate token signatures and claims against the issuer's JWKS.

**Recommended approach: built-in verification.** The server verifies RS256 signatures against the JWKS endpoint, validates `iss`, `aud`, and `exp`, and checks `nbf`/`iat`. When verifying Forge tokens (whether built-in or at an upstream proxy), **pinning the audience (`aud`) is critical for security**. All Atlassian Forge apps share the same signing keys and JWKS endpoint (`https://forge.cdn.prod.atlassian-dev.net/.well-known/jwks.json`). A token signed by Atlassian is cryptographically valid for any Forge app; only matching `aud` against your app's specific ARI (`ari:cloud:ecosystem::app/<app id>`) prevents tokens from other Forge apps from authenticating against your server.

**AWS ALB cannot verify Forge Invocation Tokens.** ALB's `jwt-validation` action requires `iss` to be a valid URL, but the FIT `iss` claim is the string `forge/invocation-token`, so the rule cannot match a real token. For Forge deployments, use the server's built-in verification.

**Alternative: reverse proxy verification (decode-only mode).** When verification flags are omitted, bearer sources operate in decode-only mode, parsing token payloads without verifying cryptographic signatures or expiration. This is safe only when `harnx-a2a-server` runs in a private network behind a reverse proxy that verifies every incoming token before forwarding requests, with nothing else able to reach the server. If unauthenticated traffic reaches the server directly in decode-only mode, callers can forge identities by sending arbitrary JWT payloads.

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

## Built-in Bearer Token Verification

`harnx-a2a-server` can verify bearer tokens directly against a remote JWKS without requiring a verifying proxy upstream.

### Verification CLI Flags

All three flags are required together to enable verification:

| Flag | Value | Description |
|---|---|---|
| `--bearer-jwks-url <URL>` | JWKS URL | Remote JWKS endpoint URL to fetch RS256 public keys. Must use `https`; `http` is only accepted for loopback hosts (testing). Redirects are never followed. |
| `--bearer-issuer <ISSUER>` | Issuer string | Expected `iss` claim in the bearer token (for Forge: `forge/invocation-token`). Must be non-empty. |
| `--bearer-audience <AUDIENCE>` | Audience string | Expected `aud` claim in the bearer token (for Forge: `ari:cloud:ecosystem::app/<app id>`). Must be non-empty. Pinning audience is essential because all Forge apps share signing keys. |

### Verification Example

```sh
harnx-a2a-server \
  --agent jira=team/triage \
  --user-id-header forge-fit \
  --bearer-jwks-url https://forge.cdn.prod.atlassian-dev.net/.well-known/jwks.json \
  --bearer-issuer forge/invocation-token \
  --bearer-audience ari:cloud:ecosystem::app/d0d52620-3203-4cfa-8db5-f2587155f0dd
```

To require a valid token, configure `forge-fit` (or `bearer-claim:CLAIM`) as the only `--user-id-header` source alongside the verification flags:
- When an `Authorization: Bearer` token is present, the verification flags ensure it is verified. An invalid, expired, or unverified token fails closed immediately (HTTP 401 / JSON-RPC `-32000`) with no fallthrough to later sources.
- If a request lacks an `Authorization` header, resolution falls through to subsequent configured sources. Any additional `header:` or `cookie:` source would admit requests lacking a bearer token. With no other sources configured, requests without an `Authorization` header fail resolution and are rejected with HTTP 401.
- Omitting `--user-id-header` entirely runs the server in shared anonymous mode without identity checks.

### Verification Behavior and Parameters

- **Algorithm**: Pinned to `RS256`. Tokens specifying any other algorithm (such as `none` or symmetric `HS256`) are rejected immediately before key lookup.
- **JWKS Transport**: The JWKS URL must use `https`; an `http` URL fails startup unless the host is loopback (`127.0.0.1`, `::1`, `localhost`), which exists for tests. Redirects are never followed, so an endpoint can't be re-pointed or downgraded to a plaintext hop.
- **JWKS Cache and Key Rotation**: Keys are fetched lazily on the first bearer request and cached by key ID (`kid`). If a token references an unknown `kid`, the server fetches a fresh JWKS. Refetches are single-flight (concurrent requests share one fetch) and throttled to at most one per 30 seconds to prevent fetch storms. A refresh replaces the entire cached key set, rejecting rotated-out keys. JWKS requests time out after 5 seconds and response bodies are capped at 1 MiB.
- **Claims Validation**:
  - `iss` must match `--bearer-issuer`.
  - `aud` must match `--bearer-audience`. Pinning `aud` is essential because all Forge apps share Atlassian signing keys.
  - `exp` is required; expired tokens are rejected.
  - `nbf` and `iat` are validated when present. Tokens issued in the future are rejected. A present `iat` that isn't a JSON number fails closed.
  - Clock-skew leeway for time claims is 10 seconds (tuned for FIT tokens with ~25-second lifespans).
- **Failure Behavior**:
  - Verification failures fail closed: HTTP 401 Unauthorized with JSON-RPC error code `-32000` and a static reason category (e.g. `bad bearer token signature`, `bearer token is expired`, `jwks fetch failed`).
  - No fallthrough: if a bearer source is selected and an `Authorization: Bearer` token is present, any verification or decoding failure rejects the request immediately. Subsequent identity sources are never tried.
  - Missing `Authorization` header or non-`Bearer` scheme falls through to the next configured identity source.
- **Logging**: JWKS infrastructure faults (fetch, body, key parse) are logged at `warn` with a reason category; per-token rejections are logged at `debug`, so forged-token noise doesn't hide an outage.
- **Redaction**: Raw tokens, claim values, and JWKS response payloads are never logged or echoed in error responses. Only the resolved identity (DN or claim string) is stored as the session or task owner.
- **Startup Validation**:
  - Setting any of `--bearer-jwks-url`, `--bearer-issuer`, or `--bearer-audience` without the others fails startup: `"--bearer-jwks-url, --bearer-issuer and --bearer-audience are all required to enable bearer token verification"`.
  - A non-https `--bearer-jwks-url` on a non-loopback host fails startup: `"--bearer-jwks-url must use https; http is only allowed on loopback hosts"`.
  - An empty or whitespace-only `--bearer-issuer` or `--bearer-audience` fails startup: `"--bearer-issuer must not be empty"` / `"--bearer-audience must not be empty"`.
  - Setting verification flags without a bearer source (`forge-fit` or `bearer-claim:CLAIM`) fails startup: `"bearer verification flags require a bearer identity source; configure --user-id-header forge-fit or bearer-claim:CLAIM"`.

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

## Interactive tool approval

When a tool requires confirmation (for example, a `PreToolUse` hook returns
`permissionDecision: ask`), the task pauses at `TASK_STATE_INPUT_REQUIRED`.
Its agent status message contains a plain-text Markdown challenge with the tool
name, persisted JSON arguments (truncated above 8 KiB), and approval reason.
Blocking `SendMessage` returns at this boundary. `SendStreamingMessage` and
`SubscribeToTask` close after the interrupted status; the task is **not terminal**.

Send another `SendMessage` or `SendStreamingMessage` to answer. Preserve the
returned `contextId`. Include the same `taskId`, or send only `contextId` to answer
that context's active challenge (as Jira/Rovo does). A `taskId`-only reply also
works; a mismatched task/context pair is rejected. Replies resume the original
task and invocation, not a new prompt. Multiple approval-requiring calls, including
calls in one tool round, produce successive challenges on that same task.

The first whitespace-delimited word selects the decision, case-insensitively:

| Decision | Accepted first words | Example |
| --- | --- | --- |
| Approve | `y`, `yes`, `go`, `approve` | `yes please proceed` |
| Reject | `n`, `no`, `reject`, `deny` | `n use a read-only command instead` |

Trailing `.`, `!`, and `,` on the first word are accepted (`Y!`, `no.`).
The rest is an optional note. Approval notes are **audit-only**, not new messages
to the model. Rejection notes are **model-visible** in the blocked-tool result.
Only text parts are accepted as decisions. Data parts, mixed text/data messages,
and unrecognized words such as `yesterday` return the challenge and reply
instructions without adding agent context, calling the model, or deciding anything.
There is no A2UI or structured JSON approval format.

For example, reply using values returned by the challenge:

```json
{
  "message": {
    "messageId": "approval-reply-1",
    "role": "ROLE_USER",
    "contextId": "<returned contextId>",
    "taskId": "<returned taskId>",
    "parts": [{"text": "y"}]
  }
}
```

Keep `messageId` unchanged when retrying the same reply. Its durable receipt is
bound to that exact challenge, so a duplicate cannot answer a later approval.
Concurrent conflicting replies are first-wins; a stale loser cannot decide the
next challenge. Use a new message ID for each new answer. Accepted replies publish
`TASK_STATE_WORKING` before the next challenge or terminal result. `CancelTask`
also works while approval is pending.

Parked tasks release A2A ownership and survive backend/fleet restarts. Replies
can reach any replica without sticky routing. The reconciler also discovers
runtime decisions made through another frontend, such as the web UI, and resumes
the original continuation. **Upgrade all A2A replicas together**: old replicas
can mistake parked tasks for interrupted work. Upgrade runtime workers before
enabling exact-reference decisions; old workers don't enforce request fencing.

**Rollback procedure**: Old server replicas (before interactive tool approval)
treat unowned parked tasks as ordinary orphans and mark them `TASK_STATE_FAILED`
with "interrupted by server restart" on startup or foreground access. Before
rolling back to an older server version, drain parked approvals: answer or cancel
every task in `TASK_STATE_INPUT_REQUIRED`, or accept that remaining parked tasks
will be failed by older replicas. Find parked tasks by calling `ListTasks` per
context with `status="TASK_STATE_INPUT_REQUIRED"` (or lenient `"input-required"`).
Because `ListTasks` is paginated, follow `nextPageToken` as `pageToken` (with
optional `pageSize`) until `nextPageToken` is empty. Cancel unhandled parked
tasks with `CancelTask`. See [rolling back server versions](../../docs/a2a-operations.md#rolling-back-server-versions).

There is no approval-specific timeout; approvals follow Web/TUI precedent.
The existing frozen invocation execution deadline (`run_limits.timeout_secs`,
24 hours by default from admission) keeps counting while parked `INPUT_REQUIRED`.
Approval does not pause, reset, or extend this deadline. On expiry, the runtime
cancels the invocation (the task transitions to `TASK_STATE_CANCELED`, not a
rejection), and late replies cannot revive it. The approval window is whatever
remains of the original invocation budget. See
[publisher segments and frozen deadlines](src/runner/README.md#hitl-publisher-segments)
and the [tool confirmation guide](../../docs/tool-confirmation-guide.md).

## Limitations

- **Multi-replica routing**: Shared admission, ownership, cancellation and streaming work through any backend without sticky sessions. Drain old turns and exclude mixed writers when upgrading; deploy runtime readers before `admission_closed` writers. See [operations](../../docs/a2a-operations.md). Recovery is eventual and TTL-based, not a fixed failover SLO. Interrupted work is never replayed and tool side effects aren't rolled back.
- **No push notifications**: Push notification methods (`CreateTaskPushNotificationConfig`, etc.) return `PushNotificationNotSupportedError` (`-32003`). `capabilities.pushNotifications` is set to `false`.
- **Anonymous mode lacks isolation**: When `--user-id-header` is not configured, all requests operate as a single anonymous principal. Any client reaching the endpoint can access or resume sessions.
- **A2A 0.3 compatibility is input-only**: The server accepts 0.3 method names, legacy `blocking`, and relaxed enum names on incoming requests, but always responds with canonical A2A 1.0 wire payloads. Full 0.3 wire responses are not supported.
- **ListTasks requires contextId**: Unscoped listing across all tasks is disabled. `ListTasks` requires a non-empty `contextId` and lists only tasks within that authorized session.
- **A2A TCK waivers**: The TCK workflow is gating, with 16 known failures listed individually in [`scripts/a2a-tck/waivers.toml`](../../scripts/a2a-tck/waivers.toml). Strict pytest xfail markers keep these tests running and report their reasons. Any unwaived failure, unexpected pass (`XPASS(strict)`), or waiver matching no collected test fails the job. When a test is fixed, remove its `[[waiver]]` entry and rerun `scripts/run-a2a-tck.sh`; see the [waiver guide](../../scripts/a2a-tck/README.md) for categories and scope.
- **protoc required at build time**: The underlying `a2a-pb` crate compiles protocol buffer schemas during the build. While release builds and CI can use vendored binaries, environments with non-executable cargo home directories require a system compiler via `PROTOC=/usr/bin/protoc`.
