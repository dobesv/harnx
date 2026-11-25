# harnx-serve

`harnx-serve` is standalone HTTP server binary for `harnx` agent harness. It is now the only supported way to run server mode, with a smaller dependency footprint that omits TUI and terminal-related components.

## Overview

The server allows external clients (such as IDE plugins or web interfaces) to interact with `harnx` agents over HTTP. It supports agent execution, session management, and MCP tool orchestration.

## AG-UI Runs

When a prompted AG-UI run is rejected by its session, the request fails with HTTP 409 and the rejection reason before any stream opens.

## Remote Agents

harnx-serve supports remote `agent@cluster` agents via the same mechanism as CLI/TUI. Agent resolution uses `AgentRef::parse` and `Config::use_agent`, which validates the cluster via `nats_servers/<cluster>.yaml` and records `remote_agent` state.

**URL encoding:** The `@` in agent names is percent-encoded. Example:
- `sisyphus@shared` → `/v1/agents/sisyphus%40shared`
- Package-qualified remote: `coding/coder@shared` → `/v1/agents/coding%2Fcoder%40shared`

**Cluster-client mode:** When `HARNX_NATS_SERVER=<cluster>` is set, the server connects as a client to that cluster. Agents on the default cluster are displayed without the `@cluster` suffix in the web UI (e.g., `sisyphus` instead of `sisyphus@shared`), since the cluster is implied by the routing. Agents on other clusters retain their suffix. This applies to agent listings, detail pages, and handoff events.

A session targeting a remote agent runs its turns on a worker in that cluster. A server addressing only remote agents never starts a local broker or worker. Per-cluster JetStream namespaces isolate storage; session storage keys use only the bare agent name plus session ID. See [`docs/nats-ha.md`](../../docs/nats-ha.md) for the `agent@cluster` convention and catalog discovery.

## Installation

To install `harnx-serve` from the `harnx` workspace:

```sh
cargo install --path crates/harnx-serve
```

## CLI Options

| Option | Short | Description |
| :--- | :--- | :--- |
| `--addr <ADDRESS>` | `-a` | Listen address (default from `config.yaml` or `127.0.0.1:8000`). |
| `--user-id-source <SOURCE>` | | Identity source (`header:NAME`, `header-csv:NAME`, `cookie:NAME`, or bare `NAME`); repeat in priority order. Replaces `serve_user_id_sources` from config when supplied. |
| `--group-header <NAME>` | | Trusted group header spec (`header:NAME`/bare `NAME` for one opaque item per field; `header-csv:NAME` for quoted lists); repeat to collect all items. Replaces `serve_group_headers` from config/environment when supplied. |
| `--role-header <NAME>` | | Trusted role header spec (same syntax as `--group-header`); repeat to collect all items. Replaces `serve_role_headers` from config/environment when supplied. |
| `--access-rules <PATH>` | | Access rules file (default: `access.yaml` in the harnx config directory; env `HARNX_ACCESS_RULES`). Enables identity-based access control. |
| `--public-url <URL>` | | URL a browser uses to reach the Web UI (default from `config.yaml`; inferred from request when unset). |
| `--model <MODEL>` | `-m` | Select a specific LLM model to use. |
| `--dry-run` | | Echo prompts instead of sending them to the LLM. |
| `--web-assets <PATH>` | | Directory of web-ui static assets to serve (default: `~/.local/share/harnx/web-assets`, XDG-aware). |

## Web UI assets

The `--web-assets` option specifies the directory where the server looks for the Web UI's static files (HTML, JS, CSS).

- **Default path**: `~/.local/share/harnx/web-assets` (XDG-aware; honors `HARNX_DATA_DIR` and `XDG_DATA_HOME`).
- **Behavior**: Assets are optional. If the directory or a requested file is missing, the server returns 404 for those paths but continues to function. `/v1/*` API routes take precedence.
- **Obtaining assets**: Each release publishes prebuilt `harnx-web-assets-<version>.tar.gz` and `harnx-web-assets-<version>.zip` archives on the [GitHub Releases page](https://github.com/dobesv/harnx/releases). Download one, extract it into `~/.local/share/harnx/web-assets`, or point `--web-assets` at the extracted directory.
- **Build from source**: If you prefer, build the UI yourself and copy it into the assets directory:

```sh
# From the repository root
cd web
pnpm install
pnpm build
mkdir -p ~/.local/share/harnx/web-assets
cp -r dist/* ~/.local/share/harnx/web-assets/
```

Alternatively, point the server directly at the build output:
```sh
harnx-serve --web-assets ./web/dist
```

## AG-UI (Agent User Interaction Protocol)

`harnx-serve` implements AG-UI as a content-negotiated, permalinkable REST surface under `/v1/agents`. SSE subscription and JSON-RPC control both use same canonical session URL.

The AG-UI surface allows modern web interfaces to interact with `harnx` agents using a real-time event stream and a JSON-RPC control plane.

Harnx session identity and AG-UI thread identity are deliberately separate.
The `:session` path segment is the canonical Harnx session ID (either
server-generated or client-chosen) used by URLs, the NATS stream, and canonical
session metadata. AG-UI's `ThreadId` type requires a UUID, so
`ag_ui::derive_thread_id` deterministically maps a session ID to UUID v5 at the
wire boundary (and passes legacy UUID session IDs through). The derived UUID is
never used as a persistence or routing key.

### Endpoints

| Method | Path | Accept / Content-Type | Purpose |
| :--- | :--- | :--- | :--- |
| `GET` | `/v1/agents` | `application/json` | List all configured agents. |
| `GET` | `/v1/agents/:agent` | `application/json` | Agent details (name, description, active sessions). |
| `GET` | `/v1/agents/:agent/sessions` | `application/json` | List sessions for the agent. |
| `POST` | `/v1/agents/:agent/sessions` | `application/json` | Reserve a server-generated short session ID. |
| `GET` | `/v1/agents/:agent/sessions/:session` | `application/json` | Legacy active-history message array. Use JSON-RPC `session/get` for the compaction-aware default snapshot. |
| `GET` | `/v1/agents/:agent/sessions/:session/history` | `application/json` | Read-only transcript page, selected by compaction or inclusive sequence range. |
| `GET` | `/v1/agents/:agent/sessions/:session/metadata` | `application/json` | Read redacted canonical session metadata. |
| `PATCH` | `/v1/agents/:agent/sessions/:session/metadata` | `application/json` | Update title, variables, or explicit session overrides. |
| `PUT` | `/v1/agents/:agent/sessions/:session/metadata/extensions/:namespace` | `application/json` | Atomically replace one extension namespace. |
| `DELETE` | `/v1/agents/:agent/sessions/:session/metadata/extensions/:namespace` | `application/json` | Delete one extension namespace. |
| `GET` | `/v1/agents/:agent/sessions/:session/events` | `Accept: text/event-stream` | Notify passive clients when any frontend updates the session. |
| `POST` | `/v1/agents/:agent/sessions/:session` | `Accept: text/event-stream` | **Subscription Plane**: SSE event stream. |
| `POST` | `/v1/agents/:agent/sessions/:session` | `Content-Type: application/json` | **Control Plane**: JSON-RPC 2.0 interface. |
| `POST` | `/v1/agents/:agent/sessions/:session/attachments` | `multipart/form-data` | Upload attachments; returns `cid:media:` refs. |
| `GET` | `/v1/agents/:agent/sessions/:session/attachments/:cid` | `image/*` | Retrieve attachment blob by content-ID. |
| `GET` | `/v1/cid/:encoded_cid` | `*/*` | Resolve `cid:` URL (bearer capability when rules are off; session-authorized when rules are on). |

### CID Resolution

`GET /v1/cid/:encoded_cid` serves content-addressed blobs addressed by canonical `cid:` URLs:
- `{encoded_cid}` is the full `cid:media:` or `cid:plan:` URL, percent-encoded.
- **Authorization**:
  - **Rules disabled**: Bearer capability; the URL itself authorizes access without session checks.
  - **Rules enabled**: The server validates caller permissions against the owning session embedded in the CID. Inline/temporary sessions (`SessionRef.agent == None`) and unauthorized callers return HTTP 404 (with no ETag leaked).
- Resolves via `harnx_blob_store::resolve`, which handles both media object store and plan KV rendering.
- **Security headers** on all responses:
  - `X-Content-Type-Options: nosniff`
  - `Content-Security-Policy: default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; sandbox`
- **Content-Disposition**:
  - `inline`: `text/plain` and non-SVG raster images (`image/jpeg`, `image/png`, `image/gif`, `image/webp`).
  - `attachment`: HTML, SVG, PDF, markdown, and all other types (forced download).
- **Caching**:
  - **Rules disabled**: `cid:media` emits `public, max-age=31536000, immutable`; `cid:plan` emits `no-cache` with ETag (`If-None-Match` returns `304 Not Modified`).
  - **Rules enabled**: Protected responses (including conditional `304 Not Modified` on plans) emit `Cache-Control: private, no-store`. Shared reverse proxies and CDNs should be purged when enabling access rules.

Errors: 400 for malformed CIDs, 404 if blob not found or caller is unauthorized.

### Attachment Retrieval

`GET /v1/agents/:agent/sessions/:session/attachments/:cid` returns attachment blob bytes:
- `{cid}` must be canonical `cid:` + 64 hex characters (URL-encoded as `cid%3A<hex>`).
- Validates CID membership in the session log before storage access — session scoping is the access control.
- When access rules are enabled, callers must also have permission to access the session.
- Reads local content-addressed cache first, falls back to NATS ObjectStore.
- MIME allowlist: `image/png`, `image/jpeg`, `image/webp`, `image/gif`. Returns `415 Unsupported Media Type` for other types.
- Headers: `Content-Type: <mime>`, `X-Content-Type-Options: nosniff`, `Cache-Control: private, max-age=86400`.

### Attachment Upload

`POST /v1/agents/:agent/sessions/:session/attachments` accepts a `multipart/form-data` body of `attachment` parts (filename plus part `Content-Type`):
- Session ID requirement: because uploaded attachments embed a `SessionRef`, `:session` must match `[A-Za-z0-9_-]+` and be at least 6 characters long.
- Accepted part types: `image/png`, `image/jpeg`, `image/webp`, `image/gif`, `application/pdf`, `text/plain`; anything else is `415 Unsupported Media Type`. The whole request is capped at 20 MiB (`413 Payload Too Large`).
- Success returns `200` with `{"attachment_refs": ["cid:media:...", ...], "attachments": [{"cid": "..."}, ...]}`.
- Each accepted part is written to the shared `harnx_attachments` NATS object store before its ref is returned; the serving pod's local copy is only that pod's warm cache. Refs are usable on any replica and survive pod restarts.
- Parts are processed in order and the first error wins; a failed request returns no refs even if earlier parts were stored (stored parts are content-addressed and harmless).
- If the object-store write fails, the response is `503` (broker unreachable) or `500` (other store error) with an `{"error": ...}` body and no refs. With access rules on, the upload additionally re-reads the session's metadata before buffering the body to prove the session still exists; a session that was deleted or replaced by then returns `404` `{"error": "session no longer exists"}` with no refs, and that read does touch the broker even for requests that later fail validation. With access rules off, validation errors (`400`/`413`/`415`) that occur before the first accepted part never touch the broker, so they are returned unchanged even when the broker is down. Once a part is accepted, the broker has been used: a later part's validation error still wins (the request returns no refs), but a broker failure storing that part can surface first.
- Sessions that have a metadata record (rules on or off) are also re-checked after each stored part: a session deleted or replaced between the put and that re-read returns `404` `{"error": "session no longer exists"}` with no refs, and the object version that part just wrote is removed from the object store (`purge_media_version`: its metadata message and that version's chunks — never a newer same-CID version and never a dedup hit). A failed re-read is not treated as deletion: it returns `503`/`500` and purges nothing. Sessions that were never created through the API store unrechecked and their blobs have no garbage collection.

### Session Listing and Pagination

`GET /v1/agents/:agent/sessions` lists sessions for the specified agent. It supports optional keyset pagination via query parameters while remaining backward-compatible with unpaginated callers:

- **Unpaginated Mode (no query parameters)**:
  - If no query parameters are provided (e.g. `GET /v1/agents/:agent/sessions`), the response is a plain JSON array of session summary objects (`[SessionRef, ...]`), preserving existing behavior and compatibility.
- **Paginated Mode (`limit` and/or `cursor` query parameters)**:
  - If `limit` and/or `cursor` are specified, the response envelope is:
    ```json
    {
      "sessions": [ ... ],
      "next_cursor": "<opaque_base64url_cursor>"
    }
    ```
  - When the final page is reached, `next_cursor` is `null`.

#### Query Parameters

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `limit` | positive integer | `50` (if `cursor` only) | Page size clamped between `1` and `200`. Values `<= 0` or non-integers return `400 Bad Request`. |
| `cursor` | string | `None` | Opaque base64url JSON token encoding the sort key of the last item from the previous page. Bounded to max 1024 bytes; must pass strict version check (`v: 1`). Malformed or invalid cursors return `400 Bad Request`. |

Duplicate `limit` or `cursor` parameters return `400 Bad Request`. Other query parameters are ignored; without `limit` or `cursor`, the response remains the legacy array.

#### Keyset ordering

Sessions sort by:
1. `modified` timestamp descending (most recently active sessions first). Exact nanosecond timestamp precision is preserved in the cursor sort key rather than truncating to formatted milliseconds.
2. Sessions with a `modified` timestamp sort ahead of sessions with `None`.
3. Ties in `modified` timestamp (including both `None`) are broken deterministically by session `id` descending.

The cursor selects sessions strictly after `(modified, id)`, not a numeric offset. Inserting or deleting sessions, including the cursor's session, doesn't skip or duplicate surviving sessions whose sort keys haven't changed. New sessions ahead of the cursor appear on the next first-page refresh.

Pages aren't a frozen snapshot. Activity can move a session across the cursor between requests. Clients should deduplicate by `session_id` and restart at the first page when reconciling live changes.

#### Note on `GET /v1/agents/:agent` (`agent_json`)

The agent detail endpoint `GET /v1/agents/:agent` embeds a `sessions` array. This embedded list remains the complete, unpaginated array of all sessions for that agent so that clients retrieving the full agent resource do not receive truncated or partial data. Incremental loading is performed exclusively through `GET /v1/agents/:agent/sessions`.

### Canonical session metadata

The metadata response contains immutable session/agent identity, creation and
activity timestamps, title state, explicit overrides, extension namespaces,
and the current KV revision. Variable values and inline agent instructions are
redacted; variables are represented by name and whether a value is set.

`PATCH` accepts a typed object containing any of `title`, `variables`, and
`overrides`. Identity, agent source, and creation time cannot be changed. A
title can be cleared with `{"title":{"value":null}}`. Extension `PUT` replaces
one namespace as a single CAS update; namespaces are limited to 64 KiB and the
combined extension map to 256 KiB. Successful writes publish a session
invalidation advisory so attached clients can reload authoritative state.

## AG-UI Support (Phase 2)

`harnx-serve` implements a two-plane communication model on one URL. Content negotiation decides whether a session `POST` becomes an SSE subscription or a JSON-RPC control call.

### Single-URL Negotiation

For `POST /v1/agents/:agent/sessions/:session`, negotiation follows this tiebreak rule:

1. If request has `Accept: text/event-stream` → SSE subscription plane.
2. Else if request has `Content-Type: application/json` → JSON-RPC control plane.
3. Else → `406 Not Acceptable`.

This rule keeps subscriber requests and JSON-RPC calls unambiguous even if both target same session URL. `GET` on same URL keeps existing behavior: `Accept: text/html` returns HTML page, otherwise server returns JSON history snapshot.

### Two-Plane Architecture

#### 1. SSE Subscription Plane
**Endpoint:** `POST /v1/agents/:agent/sessions/:session`  
**Header:** `Accept: text/event-stream`

Provides AG-UI events for a run. Unknown message roles (including `reasoning` and
`activity` echoed by AG-UI clients) are accepted and dropped before selecting a
prompt. The body's **last supported message** selects the mode:

- **Prompted run** (last message is a non-empty `user` message whose ID is not
  already present in the authoritative snapshot): a pure delta
  stream — `RUN_STARTED` → `session_attach_boundary` →
  `STEP_*`/`TEXT_MESSAGE_*`/`THINKING_*`/`TOOL_CALL_*`/`CUSTOM` → `RUN_FINISHED`
  (or `RUN_ERROR`). The stream **terminates** after the
  terminal event so the client's `runAgent()` promise resolves. No
  `MESSAGES_SNAPSHOT` is emitted (it would predate the just-sent user message).

  The request body may include `timeout_secs` to limit run duration. Positive
  values can only shorten the target agent's configured run limits; they cannot
  extend beyond the policy. Omitted, null, zero, or negative values inherit the
  agent/global timeout policy. A deadline stop ends the stream with `RUN_ERROR`;
  manual cancellation still ends with `RUN_FINISHED`.
- **Promptless join** (no non-empty trailing user message): hydrates with a
  synthetic `RUN_STARTED` → `session_attach_boundary` → `session_compaction` → `MESSAGES_SNAPSHOT`.
  For an idle session it appends
  a synthetic `RUN_FINISHED` and closes, whatever the last turn's outcome
  (`session/get`'s `last_turn` reports it); for a running or interrupted session it
  follows that run's live events through the real terminal event.

Passive clients keep a separate `GET .../events` connection open. Each
`session-updated` notification tells the client to run a promptless hydrate (or,
when the server's session actor is active, join its live AG-UI run). This keeps
the AG-UI run stream finite while allowing browser tabs to invalidate each
other's snapshots. The event feed and underlying NATS fan-out are advisory, not
history: clients rehydrate from the durable session log after notifications and
at terminal or reconnect boundaries. Non-web frontends consume the same NATS
fan-out directly and converge from that durable log as well.

Note: the request body must be a JSON object containing a `messages` array (e.g.
`{"messages":[]}`); a bare `{}` is rejected as an invalid AG-UI request.

#### 2. JSON-RPC 2.0 Control Plane
**Endpoint:** `POST /v1/agents/:agent/sessions/:session`  
**Header:** `Content-Type: application/json`

Same canonical session URL, negotiated into programmatic control.

- **`session/get`**: Returns session state and capabilities.
  ```json
  { "jsonrpc": "2.0", "id": 1, "method": "session/get" }
  ```
  **Result:** `{ "state": { … }, "history_snapshot": [...], "history_reasoning": { … }, "compaction": { … }, "last_turn": { … }, "history_warnings": [], "capabilities": { "multiClient": true, "persistence": "nats" } }`
  `history_reasoning` maps assistant row IDs to the reasoning removed from their
  `content`, or is `{}`. See [Snapshot reasoning](#snapshot-reasoning).
  `compaction` is optional and omitted until the session has been compacted. See
  [Compacted history snapshots](#compacted-history-snapshots) for its shape.

  `state` is one of six shapes, derived from the durable session log (plus the
  session lease, so a turn this server never started is still reported as
  running):
  - `{ "status": "idle" }`
  - `{ "status": "running", "run_id": "…", "started_at": "…" }` — a turn owned
    by a remote worker, or an admitted prompt waiting for its worker, reports
    plain `{ "status": "running" }`, with no run id. This means durable work is
    outstanding, not that a worker necessarily holds the lease.
  - `{ "status": "stalled", "reason": "unclaimed", "prompt_seq": 41 }` — the
    prompt at that log sequence has waited past the lease-acquisition timeout
    (`nats_lease_acquisition_timeout_secs`, default 60) without a worker
    claiming it, and no lease is held. A worker that later claims the prompt
    turns the state back to `running` on the next read, so poll or re-check
    before giving up.
  - `{ "status": "stalled", "reason": "worker_lost", "prompt_seq": 41 }` — a
    worker wrote output for that prompt's turn, then its lease disappeared and
    the grace a follower's watchdog gives a reappearing lease has passed since
    its last timestamped log write. `last_turn` still describes the last terminator, so a
    stalled turn reads as one that never finished, which is what happened.
  - `{ "status": "interrupting" }` — this server's interrupt append is in
    flight.
  - `{ "status": "interrupted", "cancel_seq": 12 }` — a `Cancel` at that log
    sequence ended the turn.
  - `{ "status": "awaiting_approval", "pending_interrupts": { … } }` — the turn
    is parked at an approval gate. It is not interrupted; the client's next move
    is a decision, not another cancel.

  A stalled turn keeps `canPrompt` and `canCancel` true: `session/prompt` can
  add input for a worker to pick up, and `session/cancel` can settle the
  stalled prompt.

  `last_turn` says how the session's most recent turn ended, from the last
  `turn_end`, `error` or `cancel` entry in its log. It is omitted until a turn
  has ended, and it stays while a later turn runs. A client that stopped
  following a run needs it: `state` reads `idle` after any outcome, and
  `history_snapshot` holds messages only, so a turn that failed partway looks
  like one that finished with a short reply. A turn that stalled with no
  terminator has no `last_turn` entry — `state.status: "stalled"` is what
  reports it.
  - `{ "outcome": "completed", "seq": 41, "ended_at": "…" }`
  - `{ "outcome": "error", "seq": 41, "ended_at": "…", "message": "…" }` — the
    worker gave up on the turn, for the reason in `message`.
  - `{ "outcome": "timed_out", "seq": 41, "ended_at": "…", "deadline": "…" }` —
    the run reached its time limit.
  - `{ "outcome": "interrupted", "seq": 41, "ended_at": "…", "requested_by": "…" }`
    — a user, parent session or frontend interrupted the turn, the same
    `cancel` that makes `state` read `interrupted`. `requested_by` is omitted
    when the cancel didn't name one.

  `seq` is the log sequence of that entry. `ended_at` is omitted for entries
  written without a timestamp.
- **`session/history`**: Reads archived transcript pages without changing the session or model context. Takes `before_compaction` or inclusive `from_seq`/`to_seq`, plus optional `limit`; subsequent pages take an opaque `cursor`. See [History paging](#history-paging).
  ```json
  { "jsonrpc": "2.0", "id": 8, "method": "session/history", "params": { "before_compaction": 15, "limit": 2 } }
  ```
- **`session/prompt`**: Sends a new user prompt, creating the session if it doesn't exist.
  ```json
  { "jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": { "text": "hello" } }
  ```
  **Result:** `{ "status": "accepted", "run_id": "...", "message_id": "..." }` (idle) or `{ "status": "enqueued", "run_id": "...", "message_id": "..." }` (running). `message_id` is the AG-UI wire ID of the durable user row, so clients can reconcile an acknowledged prompt with a later transcript snapshot. It is `null` only for test executors without durable admission.

  Optional `timeout_secs` parameter limits run duration. Positive values can only
  shorten the target agent's configured run limits policy; they cannot extend
  beyond it. Omitted, null, zero, or negative values inherit the agent's default.
  ```json
  { "jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": { "text": "hello", "timeout_secs": 300 } }
  ```

  Optional `attachment_refs` parameter supplies `cid:media:` attachment refs
  (as returned by the upload endpoint) with the prompt (attachment-only prompts
  still require a `text` field, which may be `""`):
  ```json
  { "jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": { "text": "what is this?", "attachment_refs": ["cid:media:..."] } }
  ```
  When access rules are configured, each newly supplied `cid:media` ref is
  authorized against its source session before anything is admitted: the
  caller needs the same access to the session that uploaded the attachment
  that the session routes require — matching owner (including alias
  expansion) or admin scope on the source agent, with the source agent
  normalized for the destination cluster (bare for the default cluster,
  `agent@cluster` otherwise). For an existing, accessible session, at most 32
  `cid:media` refs are accepted per prompt; a longer list is rejected as
  `-32602` (HTTP 400) before any broker work, and prompts without `cid:media`
  refs do no broker work at this layer at all. An unauthorized or
  unattributable source (no record, no named agent, or a record that cannot be
  decoded) returns the generic `-32005` error below for the whole list, before
  the session log gains any entry or a turn is activated. If the source
  session's metadata cannot be read because the broker is unavailable — the
  open, or a read classified as broker unavailability — the prompt fails
  closed with `-32003` (HTTP 503), logged server-side at `warn`, so an outage
  is not mistaken for a denial. Admitting an authorized ref lets the
  destination session use its content. Same-session refs, a caller's own
  cross-session refs, and admin callers keep working; without access rules a
  ref remains a bearer capability. When access rules are enabled and the target
  session does not exist yet, attachment validation runs before metadata
  creation: unauthorized, malformed, or `>32` `cid:media` refs all return the
  guard's HTTP `404 Not Found` (not `400` / `-32602` or `-32005`), and broker
  unavailability returns HTTP `503 Service Unavailable`, leaving no session
  created.
- **`session/cancel`**: Interrupts the session's current turn by appending one
  durable `Cancel` to its session log. Takes no parameters — an interrupt always
  targets whatever turn is running. It returns as soon as that append is
  acknowledged, not when the worker has finished stopping its tools.
  ```json
  { "jsonrpc": "2.0", "id": 3, "method": "session/cancel" }
  ```
  **Result:** one of
  - `{ "outcome": "idle" }` — no turn was running, so nothing was appended.
  - `{ "outcome": "accepted", "cancel_seq": 12 }` — the `Cancel` landed at that
    log sequence.
  - `{ "outcome": "already_interrupted", "cancel_seq": 12 }` — a `Cancel`
    already terminates this turn. Repeating the call is harmless.
- **`session/compact`**: Requests manual compaction of the session's conversation history.
  Takes no parameters.
  ```json
  { "jsonrpc": "2.0", "id": 4, "method": "session/compact" }
  ```
  **Result:** one of
  - `{ "status": "submitted", "compaction_id": "..." }` — compaction request submitted to the session log.
  - `{ "status": "already_in_flight", "compaction_id": "..." }` — attached to an existing compaction operation.
  - `{ "status": "nothing_to_do", "outcome": { ... } }` — tail log indicates compaction is unnecessary. `outcome` is an adjacent-tagged `CompactOutcome`:
    - `{ "status": "compacted" }` — session was recently compacted.
    - `{ "status": "unchanged", "detail": "no_user_messages" | "nothing_eligible" | "already_compacted" }` — nothing to compact for the given reason.
    - `{ "status": "failed", "detail": "..." }` — prior compaction attempt failed with the error message in `detail`.

**Error Codes:**
- `-32001`: Unknown session (HTTP 404) for operations that require an existing session, such as `session/get`, `session/history`, `session/cancel`, `session/compact`, and read-state updates. `session/prompt` creates the session if absent.
- `-32003`: Session actor or history storage unavailable (HTTP 503). Transient: retry the call. For existing sessions, broker outages during attachment source checks also return `-32003` (HTTP 503). On unseen IDs, broker outages during metadata read/reservation or pre-reservation attachment checks instead return HTTP `503` with the guard's JSON error body, not JSON-RPC `-32003`.
- `-32004`: Prompt rejected (HTTP 409), including a prompt on an existing session whose attachment refs cannot be resolved or parsed (on an unseen ID under access rules, pre-reservation attachment rejections return the guard's HTTP 404 instead).
- `-32005`: Attachment not found (HTTP 404): with access rules configured, a `session/prompt` attachment ref on an existing session whose source session the caller cannot access, or that has no readable, decodable owner record. Generic by design; it does not reveal whether the referenced object exists. (On an unseen ID, unauthorized, malformed, or `>32` `cid:media` refs return the guard's HTTP 404.)
- `-32601`: Method not found
- Standard JSON-RPC 2.0 codes (-32700, -32600, -32602); with access rules configured on an existing session, `session/prompt` also returns `-32602` (HTTP 400) for more than 32 `cid:media` refs (on an unseen ID, `>32` `cid:media` refs return the guard's HTTP 404 instead).

`session/cancel` on an idle session is not an error; it returns
`{ "outcome": "idle" }`.

### History paging

Use `session/history` on the canonical session URL to browse a transcript window.
It doesn't append entries, admit prompts, mark the session read, or re-inject
archived messages into the model context. Authentication, agent visibility, and
session ownership checks are the same as `session/get`, including cached pages.

Initial params select exactly one window:

- `{ "before_compaction": 15, "limit": 2 }`: the view immediately before the
  `Compress` at physical sequence 15. Later edits and rewinds don't change this
  historical view. It starts with the previous compaction marker, if any.
- `{ "from_seq": 5, "to_seq": 14, "limit": 2 }`: inclusive sequence bounds within
  one window. Bounds must satisfy `0 < from_seq <= to_seq` and lie within the
  stored log's extent. Gaps and control-only ranges can return empty pages.
  A range crossing a compaction boundary is rejected; open each archive using
  `before_compaction` instead. A marker exactly at `from_seq` is included.

`limit` defaults to 100 and accepts 1–1000. It counts **logical groups**, not
physical log entries or AG-UI rows: one ordinary message, one marker, or one
complete tool round. A round can expand into an assistant call row and several
result rows, all on the same page. Selecting either half of a call/result pair
includes the entire round. Edited replacement rows sharing a physical sequence
keep their replay order and remain independently pageable. Pages are oldest-first.

Example result for `before_compaction: 15, limit: 2` using the shared fixture:

```json
{
  "jsonrpc": "2.0",
  "id": 8,
  "result": {
    "history_reasoning": {},
    "history_snapshot": [
      {
        "role": "assistant",
        "id": "a0b1fe9f-c77c-568a-8ffc-7272dd4a5cc3",
        "content": "The session log stores tool calls and results separately. No messages were kept."
      },
      {
        "role": "user",
        "id": "3e3bc161-3752-5646-a160-c7da8062e7c0",
        "content": "Implement the shared transcript view and verify paging."
      }
    ],
    "compaction": {
      "message_id": "a0b1fe9f-c77c-568a-8ffc-7272dd4a5cc3",
      "compaction_seq": 5,
      "summary": "The session log stores tool calls and results separately. No messages were kept.",
      "from_seq": 1,
      "to_seq": 4,
      "kept_count": 0
    },
    "next_cursor": "<opaque cursor>",
    "prev_cursor": null,
    "earlier_compaction": 5
  }
}
```

`history_snapshot` uses the same AG-UI shape as `session/get`. `compaction` uses
[the same metadata](#compacted-history-snapshots) and is omitted on pages without
the synthetic marker. Match the marker by `compaction.message_id`, not an ID prefix.
`earlier_compaction` identifies the marker whose archive can be opened next and
is present on **every** page of this window, even if the marker isn't in the
selected range/page. It is `null` when there is no earlier marker. Here, call
`before_compaction: 5` to reach the original first prompt and reply.

Pass `next_cursor` or `prev_cursor` back without the original selector:

```json
{ "jsonrpc": "2.0", "id": 9, "method": "session/history", "params": { "cursor": "<opaque cursor>" } }
```

Cursors are session/cluster scoped and freeze the original raw-log boundary.
Appends, edits, and rewinds after that boundary don't move subsequent pages.
Keep cursors opaque. They aren't authorization tokens. Don't combine a cursor
with sequence selectors; if `limit` is supplied again it must equal the original
limit. Cursors are `null` at the first/last page boundary. Empty pages have both
cursors `null`. A bounded server cache avoids full-log reload/replay for sequential
pages; eviction or another server can rebuild the frozen page from the durable
log. Deleting/recreating the session invalidates its old cursors.

HTTP equivalent, with the same parameters as query strings:

```http
GET /v1/agents/plain/sessions/my-session/history?before_compaction=15&limit=2
Accept: application/json
```

HTTP success returns the bare `result` object above with `Cache-Control: private, no-store`. This is a new route;
`GET /v1/agents/:agent/sessions/:session` keeps its legacy message-array response.
HTTP errors use `{ "error": { "message": "...", "type": "invalid_request_error" } }`.
JSON-RPC errors use `-32602` with an explanatory `error.data.detail` for invalid
selectors, unknown/non-Compress compaction sequences, out-of-range bounds, and
invalid/wrong-session cursors (HTTP 400). Missing sessions use `-32001` (HTTP 404),
and unavailable history storage uses `-32003` (HTTP 503). Access rules can hide
unauthorized or missing sessions behind the existing HTTP 404 response before
RPC dispatch. Unauthenticated requests retain the server's HTTP 401 behavior.

### Snapshot reasoning

`session/get`, `session/history`, and `GET /v1/agents/{agent}/sessions/{session}/history`
return `history_reasoning` beside `history_snapshot`. It maps IDs of assistant
rows in that response to arrays of reasoning strings, in transcript order.
`session/history` only includes entries for the requested page. A response
with no reasoning includes `"history_reasoning": {}`.

Example result (other fields omitted):

```json
{
  "history_snapshot": [
    {"role":"assistant","id":"d951259d-1bb5-4ea1-824f-85d5dfd28d13","content":"The tests passed."}
  ],
  "history_reasoning": {"d951259d-1bb5-4ea1-824f-85d5dfd28d13":["Check the test results."]}
}
```

On an AG-UI SSE `POST` to the session URL, send
`x-harnx-snapshot-features: history-reasoning` to opt in. The header accepts
comma-separated feature tokens. An opted-in stream sends this companion
immediately after each `MESSAGES_SNAPSHOT`:

```text
data: {"type":"CUSTOM","name":"history_reasoning","value":{"d951259d-1bb5-4ea1-824f-85d5dfd28d13":["Check the test results."]}}

```

`value` is the map itself. Replace prior snapshot reasoning with the new map,
including `value: {}` when the snapshot has no reasoning. Match keys to IDs in
the immediately preceding snapshot, not to IDs from another request or page.
The companion is omitted without the header.

In both cases, assistant snapshot `content` contains the answer without leading
reasoning wrappers written by the server or model adapters. An assistant row
with only reasoning keeps `content: ""`. Reasoning does not add snapshot rows
or affect cursors. Live `THINKING_*` events remain separate.

### Client Implementation Flow

1. **Create**: Clients can either reserve a server-generated short ID via `POST /v1/agents/{agent}/sessions` or choose their own session ID and create it by sending the first prompt to `POST /v1/agents/{agent}/sessions/{session}` (with or without access rules):
   - **JSON-RPC (`Content-Type: application/json`)**: send `session/prompt` with nonblank `text` or non-empty `attachment_refs`, and no `resume` entries. Attachment-only prompts still require the `text` field, which may be `""`.
   - **SSE (`Accept: text/event-stream`)**: send an AG-UI run body with no `resume` decisions whose last supported message (after dropping unknown roles) is a nonblank `user` message.
   - **Session ID format**: Under access rules, client-chosen `:session` IDs must be a safe path segment with no leading or trailing whitespace and satisfy the runtime's `SessionRef` format (`[A-Za-z0-9_-]+`, at least 6 characters); refused IDs (such as `demo` or `pr.12`) receive the guard's HTTP `404`. Without access rules, `:session` on the session prompt route is not checked by `claimable_session_id`, while uploading attachments (`POST /v1/agents/{agent}/sessions/{session}/attachments`) always requires the `SessionRef` format `[A-Za-z0-9_-]+` of at least 6 characters.
   - **Access rules**: Creating a session requires `prompt` scope (`admin` alone returns HTTP `403`, `session creation requires prompt scope`) and stamps the caller as the session's immutable owner. Any non-creating request on an unseen ID returns HTTP `404 Not Found`.
2. **Connect**: Open the session event feed and use promptless AG-UI runs to hydrate or join an active run. An attach after prompt admission waits for a worker or durable completion even before the worker claims the lease. If no worker claims it within `nats_lease_acquisition_timeout_secs` (default 60 seconds), the stream closes with `RUN_ERROR`, not a false `RUN_FINISHED`. This observation timeout doesn't cancel the prompt. Restore the worker and attach again, or explicitly cancel the pending turn; unanswered durable input remains `running` until completed, failed, cancelled, or retracted.
3. **Drive**: Use JSON-RPC on the same canonical session URL to send prompts
   (`session/prompt`) and interrupt runs (`session/cancel`).
4. **Stateless UI**: Clients only send new inputs via RPC; they do not need to re-POST full transcript.

### Disconnect Semantics (D5)

SSE connections are decoupled from execution. Dropping an SSE connection (e.g., reloading page) **does not stop** a running agent. Run continues to completion and persists. Only `session/cancel` or terminal error stops a run.

### Divergence from AG-UI Standards

This implementation deliberately diverges from generic AG-UI/assistant-ui standards for optimization:
- **Last-Message Inspection:** Decision to start a run is based on the last supported message in the SSE POST body (unknown roles are dropped first).
- **Two-Plane Control:** Uses JSON-RPC instead of standard RESTful run endpoints to support mid-run injection.
- **Single Canonical URL:** Both planes share one session permalink and rely on content negotiation instead of sibling routes.
- **Message-ID Replay Guard:** A trailing user row already present in the
  authoritative snapshot is hydration, not a new prompt. There is no general
  request-version or sequence reconciliation protocol.

### Client Lifecycle Verifier Constraints

The `@ag-ui/client` verify layer enforces strict lifecycle pairing: every
`TEXT_MESSAGE_END`/`TOOL_CALL_END`/`STEP_FINISHED` requires a matching `START`
on that subscriber's SSE stream, and `RUN_FINISHED`/`RUN_ERROR` must not fire
while any text/tool/step/thinking segment is open. A `MESSAGES_SNAPSHOT` hydrates
message content but does **not** update the verifier's active-lifecycle sets.

This has significant implications for attach-mid-run paths (local join and
remote-follow):

- **Per-subscriber guard required:** Each new SSE subscriber needs its own
  `LiveStreamGuard` (`ag_ui_lifecycle.rs`) to synthesize missing opens before
  unmatched ends and to finalize open lifecycles before terminal frames.
- **Latch before terminal:** `RUN_FINISHED`/`RUN_ERROR` must be preceded by
  synthesized closes for any lifecycle segments still open in that subscriber's
  guard (text message, step, tool call started on this stream, thinking).
- **Messages snapshot resets guard:** When a live `MESSAGES_SNAPSHOT` replaces
  a lagged broadcast stream, the guard must finalize any open lifecycles first
  so subsequent END events don't appear orphaned.
- **Generation-aware queue:** Remote-follow retains the attached prompt's
  generation through its backpressured event queue. Accepted root interruption
  ends the run without waiting for lease release or `TurnEnd`; a replacement
  generation cannot prolong that run or send its events under the old run ID.
  The lifecycle guard runs at wire emission, after stopped-generation events are
  discarded. It closes only lifecycles actually sent, so discarding a queued
  START cannot leave an orphan END.

See `ag_ui_lifecycle.rs` for the guard implementation and
`ag_ui_remote_follow.rs` for remote-follow integration. Tests in
`ag_ui_lifecycle_tests.rs` verify strict verifier invariants.

### Phase B Scope

Cross-process live synchronization and durable session persistence use NATS.


### Compacted history snapshots

`session/get` returns the shared default transcript view: the latest compaction
summary, followed by kept messages and later activity. Archived messages aren't
rendered inline. Even a compaction that kept no messages has one summary row.
The summary is an ordinary AG-UI assistant message with string `content`, so
history consumers that only read `role` and `content` can display it unchanged.

Example result excerpt after the first compaction in the shared history fixture:

```json
{
  "history_snapshot": [
    {
      "id": "a0b1fe9f-c77c-568a-8ffc-7272dd4a5cc3",
      "role": "assistant",
      "content": "The session log stores tool calls and results separately. No messages were kept."
    }
  ],
  "compaction": {
    "message_id": "a0b1fe9f-c77c-568a-8ffc-7272dd4a5cc3",
    "compaction_seq": 5,
    "summary": "The session log stores tool calls and results separately. No messages were kept.",
    "from_seq": 1,
    "to_seq": 4,
    "kept_count": 0
  }
}
```

`message_id` is a deterministic UUIDv5 using the existing `wire_message_id`
namespace (URL namespace, name `harnx-message:compaction:<compaction_seq>`).
Identify the summary row by matching `compaction.message_id`, not by an ID
prefix. `summary` excludes the trailing runtime recovery note. `from_seq` and
`to_seq` are inclusive archive bounds, excluding compaction markers; an empty
archive has `from_seq > to_seq`. Optional `earlier_compaction` identifies the
previous compaction. Optional `kept_count` is omitted when unknown; clients
mustn't rely on it being present.

Every live, attach, and lag-recovery snapshot is preceded immediately by:

```json
{
  "type": "CUSTOM",
  "name": "session_compaction",
  "value": { "compaction": null }
}
```

For a compacted session, replace `null` with the same `compaction` object as
`session/get`; the following `MESSAGES_SNAPSHOT.messages` contains the same
summary-first view. `null` clears stale compaction metadata after a session
switch or rewind. `session/get` instead omits the field when uncompacted.
Prompted attaches still omit the initial snapshot and metadata pair.

`session_compacting_completed` retains `outcome` and optional `compaction_id`,
and adds optional `summary`, `from_seq`, `to_seq`, and `compaction_seq` from
the current view. The completed event precedes `session_compaction` and
`MESSAGES_SNAPSHOT`. Without an available compaction marker, those four fields
are omitted.

### AgentEvent → AG-UI mapping

| harnx `AgentEvent` | AG-UI event(s) | Notes |
| :--- | :--- | :--- |
| `Model::MessageChunk` | `TEXT_MESSAGE_CONTENT` | `TEXT_MESSAGE_START` / `TEXT_MESSAGE_END` follow the assistant message lifecycle. |
| `Model::Final` | `THINKING_*` (when needed), then `TEXT_MESSAGE_CONTENT` | If no text streamed for this model call, split leading writer-shaped `<think>` wrappers from the answer. Emit their reasoning only if no thought chunks streamed for that call. |
| `Model::ThoughtChunk` | `THINKING_START`, `THINKING_TEXT_MESSAGE_START`, `THINKING_TEXT_MESSAGE_CONTENT`, `THINKING_TEXT_MESSAGE_END`, `THINKING_END` | Consecutive thought chunks share a segment. |
| `Tool::*` | `TOOL_CALL_START`, `TOOL_CALL_ARGS`, `TOOL_CALL_END`, `TOOL_CALL_RESULT` | Progress/update still dropped as too noisy. |
| `Turn::Started` / `Turn::Ended` | `STEP_STARTED` / `STEP_FINISHED` | Step names use `turn-N`. |
| `Turn::SubAgentProgress` | `CUSTOM` | Name: `sub_agent_progress`. The invocation ID correlates reused child sessions, and `tool_call_id`, when present, names the parent tool call that started the child; progress carries running/done/failed status, elapsed milliseconds, direct token usage, direct tool-call count, and optional `title` (present when the sub-agent reported a session title). |
| `Turn::RetryAttempt` / `ModelFallback` / `HandoffRequested` | `CUSTOM` | Names: `turn_retry_attempt`, `turn_model_fallback`, `turn_handoff_requested`. A handoff request is informational; clients must not navigate on it. Navigation uses the static handoff row from the tool result marker or the confirmed `session_handoff` event. |
| `Session::HandoffCommitted` | `CUSTOM` | Name: `session_handoff`. Emitted only after the target prompt is accepted for dispatch, with nonempty `agent`, resolved `session_id`, optional `handoff_tool_call_id`, and optional durable `after_seq`. Hydration always sets `after_seq` from the handoff log entry's sequence. |
| Attach boundary | `CUSTOM` | Name: `session_attach_boundary`. Emitted immediately after `RUN_STARTED` with `attached_seq` set to the durable log tail observed at attach time, before snapshot, hydrated control, or live events. |
| `Session::Compacting*` | `CUSTOM` (+ `MESSAGES_SNAPSHOT` on completed) | Names: `session_compacting_started`, `session_compacting_completed`, `session_compacting_failed`. Completion adds optional `summary`, `from_seq`, `to_seq`, `compaction_seq`, then emits `session_compaction` metadata and re-snapshots the transcript. |
| `Session::TitleUpdated` / `TitleGenerationFailed` | `CUSTOM` | Names: `session_title_updated`, `session_title_generation_failed`. |
| `Session::Saved` / `AgentInitializing` / `ModelChanged` / `RagIndexing` / `Generic` | `CUSTOM` | Stable names prefixed with `session_...`. |
| `Session::LogSeqAssigned` | dropped | Persistence bookkeeping for local transcript patching; not useful on AG-UI wire. |
| `Plan { entries }` | `CUSTOM` | Name: `plan`. Carries serialized plan entries for plan/todo panels. |
| `Status(StatusLine)` | dropped | Spinner/status chatter is high-frequency and not durable transcript structure, so server keeps it off wire. |
| `Model::Usage` | `CUSTOM` | Name: `usage`. Carries input/output/cached/cache-write/session label for token-cost displays. |
| `Notice::Error` / `Model::Error` | `RUN_ERROR` | Terminal user-visible error path. |

Intentionally dropped today:
- `Tool::Progress` / `Tool::Update` — high-volume progress noise; clients still get durable start/result framing.
- `Status(StatusLine)` — spinner/status chatter is frequent and not durable transcript structure.
- `Notice::Info` / `Notice::Warning` — not currently emitted in server flows worth surfacing; omitted to avoid custom-event spam.
## Operational Notes

### Tool Confirmation and Interaction Interrupts

When tool middleware requests confirmation (`confirm`), or when replaying legacy
tool approval requests, the lease-holding worker persists durable interaction
requests (`InteractionRequested` or legacy `HitlApprovalRequested`). The server
derives `Interrupted` state from this durable log, including after reconnects and
server restarts. Decisions are routed to the worker, which writes `InteractionResponse`
(or legacy `HitlApprovalDecision`) using lease fencing, stream-tail compare-and-set,
and ownership revalidation. Only a winning durable decision can resume the tool
round; the server does not retain a continuation.

#### Wire format

- **Web UI / JSON-RPC**: Submit one tool decision at a time using
  `session/hitl_decision` with params
  `{tool_call_id, approved, note?}`. The result is `{applied: true}` when the worker
  acknowledges the decision or the durable log already contains the same approval
  for that request. Retries recover a lost acknowledgement without executing the
  tool twice. A conflicting decision or an unresolved request that is no longer
  pending returns `{applied: false}`. Decisions from an older tool round do not
  authorize a reused tool-call ID in a newer round. Clients remove the resolved
  gate and refresh from the durable log; errors leave it available for retry.
- **Legacy SSE resume path**: The client sends a prompted run with `resume: [{interruptId, status, payload: {approved}}]`
  in the AG-UI input. `interruptId` identifies the pending tool call.
- **Legacy JSON-RPC resume path**: Same resume field in `session/prompt` params. Both legacy paths parse via
  `interrupt_resume.rs::parse_resume_params`.

Status/payload pairs:
- `"resolved"` or `"approved"` + `approved: true` → approved
- `"cancelled"`, `"denied"`, or `"rejected"` + `approved: false` → denied

The parser rejects inconsistent status/payload pairs. Each decision is routed
individually using its tool-call ID; approval no longer requires a complete batch
or a matching in-memory run ID.

Legacy resume decisions also route to the worker; they do not replay a saved prompt.

#### Cancel during a decision

`session/cancel` clears queued prompts, signals the active run, and appends the
durable `Cancel`. That `Cancel` ends the turn, and with it the turn's pending
approval requests: a decision that arrives afterwards still routes and still
returns, but with `applied: false`, because there is no longer anything waiting
on it. An approval already being routed when the interrupt lands completes
independently and returns its own result.

Cancelling is not a denial. It does not retract an approval, imply rejection, or
roll back a decision or tool side effect that already committed. To reject a
pending tool approval, submit `approved: false`.

#### Framing for reconnect

When a promptless subscribe joins a session in `Interrupted` state, the server synthesizes
a terminal `RUN_FINISHED` with `outcome` at the top level of the framed SSE event:

```json
{"type":"RUN_FINISHED","threadId":"...","runId":"...","outcome":{...interrupt metadata...}}
```

This is visible to the `@assistant-ui/react-ag-ui` event parser, which reads `payload.outcome`
and ignores `result`. The client's `BatchInterruptUI` renders the approval gate, and the
decision is submitted through `session/hitl_decision`. The outcome is NOT nested under `result`.

The `SubscribeResult` from `SessionCommand::Subscribe` atomically carries `state: SessionState`
so a reconnecting client sees the correct interrupt snapshot without a subscribe/get-info race.

- **Persistence and `--dry-run`:** NATS transcript writes are skipped in `--dry-run` mode. A generated session ID is still reserved with canonical NATS session metadata.
- **Durable Refresh:** History reflects the latest durably appended log entries.
  Live notifications are advisory; clients reload the authoritative transcript
  after each notification and on reconnect.

### Middleware confirm interactions

Middleware confirmations use a separate exact-generation API. The legacy
`session/hitl_decision` method and `hitl_pending_approval` event are unchanged.

The server emits an AG-UI `CUSTOM` event named `pending_interaction` only after
its round's durable `SuspensionReady` manifest exists:

```json
{"type":"CUSTOM","name":"pending_interaction","value":{"interaction_id":"...","request_seq":42,"tool_call_id":"...","kind":"confirm","message":"Run this tool?","pending_count":1}}
```

`request_seq` is the sequence of `InteractionRequested`, not `SuspensionReady`.
Only the first unresolved interaction in manifest order is exposed;
`pending_count` counts further queued interactions. An accepted response exposes
the next request. A null `value` clears the card when nothing remains. Reconnects
rebuild this state from the log. A modal boundary ends the current AG-UI attachment
with `RUN_FINISHED` without completing the suspended invocation. Session state is
`awaiting_approval`, with metadata `{type: "interaction", pending: <event value>}`;
this is not a legacy `tool_call` interrupt and must not use legacy resume routing.

Submit a response to the owning session's JSON-RPC endpoint:

```json
{"jsonrpc":"2.0","id":1,"method":"session/interaction_response","params":{"interaction_id":"...","request_seq":42,"response":{"action":"approve","data":{},"note":"Checked the command"}}}
```

For `confirm` interactions, `action` is `approve` or `deny` with empty `data` and an optional string `note`. For `a2ui` interactions, `action` is any button action name declared on the surface with bound input `data` (or `deny` to dismiss) and an optional string `note`.
The exact interaction ID and positive request sequence are required. Unknown
fields and malformed responses return `-32602`. Normal session authorization
runs before this method; clients never supply a worker fence.

Acceptance uses the worker's durable first-wins CAS before waking the suspended
call. A new answer returns `{applied: true, outcome: "applied"}`; an identical
retry returns `{applied: true, outcome: "already_applied"}` without another append.
Conflicting, mismatched or stale responses return JSON-RPC error `-32004`, message
`interaction response rejected`, and data `{outcome: "conflicting" | "wrong_sequence"
| "stale" | "after_complete" | "lease_lost"}`. Transport failures use `-32003`.
Clients must show rejected answers as errors, not treat them as successful denials.
When readable, a conflict includes the recorded answer in `error.data.response`
(`{action, data, note?}`). A note change can conflict even when the action matches,
so clients must not infer that the recorded action is the opposite of their answer.

Web shows the message with Approve/Deny and an optional note. Unsupported kinds
show `not supported by this client` and Deny only. Interactions queue behind any
legacy approval card so only one request is shown at a time.

### Answer a direct child from the parent page

`sub_agent_progress` custom events carry optional `pending_interaction` summaries
in live updates and durable catch-up. A nonterminal row with a summary is awaiting
approval. The summary names the owning child agent, routing cluster and session,
inner tool, bounded message and exact request identity. The child owns the request;
the parent session doesn't suspend or proxy answers.

For legacy approvals, post the displayed reference to the **child's** session URL:

```json
{"jsonrpc":"2.0","id":1,"method":"session/hitl_decision_exact","params":{"request":{"invocation_id":"...","tool_round_seq":10,"request_seq":11,"tool_call_id":"..."},"approved":true,"note":"Reviewed the request"}}
```

Normal agent/session access checks run against that child before any decision.
The result has `applied` and a required `outcome`:

- `applied`: `applied: true`; clear the row's pending state.
- `already_applied`: `applied: true`, plus recorded `approved` and `note`. The same
  boolean answer is a successful retry; a different answer is shown as answered
  differently elsewhere.
- `conflicting`: `applied: false`, plus recorded `approved`; show that answer.
- `stale`: `applied: false`; this request is no longer pending. A newer request
  that reuses the same tool-call ID is untouched.

A missing or unrecognized outcome is an error, including `{applied: true}` alone.
If the method is unknown, open the child session to answer there. Never fall back
to `session/hitl_decision`, which intentionally selects the current generation
for existing session-page callers. That legacy method and its `{applied}` result
are unchanged.

For middleware `confirm` summaries, use `session/interaction_response` on the
child URL with the displayed `interaction_id` and `request_seq`. Other kinds have
an open-session link but no parent-row answer buttons.

### Non-modal A2UI surfaces and actions

Non-modal A2UI surfaces accompany completed tool results (`ToolResults.user_surfaces`). Unlike modal interactions, non-modal surfaces do not suspend the session or block the composer; multiple surfaces can remain active simultaneously.

#### `a2ui_surface` custom event

During promptless attach and live catch-up, `harnx-serve` emits an AG-UI `CUSTOM` event named `a2ui_surface` for each surface published in durable tool results:

```json
{"type":"CUSTOM","name":"a2ui_surface","value":{"surface_id":"deploy-form","publish_seq":42,"tool_call_id":"call_1","payload":{"version":"1.0","createSurface":{}}}}
```

When an action on a surface is accepted, the server emits another `a2ui_surface` event carrying the resolution:

```json
{"type":"CUSTOM","name":"a2ui_surface","value":{"surface_id":"deploy-form","publish_seq":42,"action":"deploy","visibility":"note","note_text":"Deploying release"}}
```

The marker identity is `(surface_id, publish_seq)`. Frontends deduplicate live and replayed events using this pair.

#### Submit a surface action (`session/surface_action`)

Frontends submit actions by calling the JSON-RPC method `session/surface_action`:

```json
{"jsonrpc":"2.0","id":1,"method":"session/surface_action","params":{"surface_id":"deploy-form","publish_seq":42,"response":{"action":"deploy","data":{"target":"staging"},"note":"Deploy to staging"},"user_message":"Deploy to staging"}}
```

Parameters:
- `surface_id`: non-empty string identifying the surface.
- `publish_seq`: positive integer of the enclosing `ToolResults` log sequence.
- `response`: `{ "action": string, "data": object, "note"?: string }` built through the shared response contract.
- `user_message`: optional string used as model-facing text for `note` or `turn` visibility. The backend reads `user_message` and does not use `response.note` for model input.

Outcomes:
- On success: Returns `{ "applied": true, "outcome": "applied" | "already_applied" }`. A retry with identical action and data returns `already_applied` without appending a new record. Resubmitting identical action and data with a different note is also treated as `already_applied`, and the second note is dropped.
- On rejection: Returns JSON-RPC error `-32004` (`surface action rejected`) with data `{ "outcome": "<reason>" }`, where `<reason>` is one of:
  - `conflicting`: a different action was already recorded for this generation.
  - `unknown_surface`: no published surface matches `surface_id` at `publish_seq`.
  - `undeclared_action`: the action is not a declared button on the surface (`deny` is also rejected; non-modal surfaces have no dismissal action).
  - `invalid_visibility`: the button declares an unknown visibility value.
  - `invalid_tool`: the declared target tool is malformed.
  - `invalid_data`: submitted data keys or types conflict with surface input bindings.
  - `unauthorized_tool`: the direct tool target is not permitted for this session.
  - `tool_failed`: the direct target tool executed and returned a failure.
  - `lease_lost`: the worker lost its session lease during execution. A direct tool may have run; check the session before retrying.
  - `append_failed`: the action record could not be confirmed after repeated CAS conflicts or a handler panic. A direct tool may have run, and the record may already be durable; check the session before retrying.
- On malformed arguments: Returns JSON-RPC error `-32602` (`invalid params`).
- On routing failure: Returns JSON-RPC error `-32003`.

Execution semantics:
- Direct tools follow an execute-then-record order: the tool runs before the action record is appended so its summary can be quoted in the note. If a crash happens after the tool runs but before the record lands, a retry will run the tool again (execute-then-record window).
- At-most-once tool execution is owner-local: guaranteed by the active worker's in-memory gate. Submissions to an idle session without an active worker are not cross-process serialized; concurrent idle submissions may run a non-idempotent tool more than once before the first record commits.
- Frontends and `harnx-serve` must upgrade before workers: older clients do not recognize the worker's early `Pending` acknowledgement and fall back to idle execution after 40 seconds, risking a duplicate tool run.

## Request Identity

`harnx-serve` can associate incoming requests with an opaque user identity string, storing it in canonical session metadata (`user_id`).

### Configuration

Identity sources can be configured via `serve_user_id_sources` in `config.yaml`, the `HARNX_SERVE_USER_ID_SOURCES` environment variable, or repeated `--user-id-source` CLI flags. CLI flags replace configuration file sources when supplied.

Each source is an ordered entry:
- `header:NAME` — reads one opaque, trimmed value from HTTP request header `NAME` (case-insensitive header name); no comma splitting or unquoting.
- `header-csv:NAME` — parses a quoted comma-separated list and selects its first item (legacy first-item identity behavior). An empty first item or malformed list fails closed. See [Group and Role Memberships](#group-and-role-memberships) for quoting syntax; the parser is shared.
- `cookie:NAME` — reads cookie `NAME` from `Cookie` request headers (case-sensitive cookie name).
- `NAME` (bare name) — alias for `header:NAME`.

Sources are evaluated in declaration order. The first present source wins:
- If the first matching source contains a valid, non-empty value, that value becomes the request's user identity.
- **Fail closed**: If the first matching source is present but empty or malformed, `harnx-serve` immediately rejects the request with HTTP `401 Unauthorized`. It does not fall back to subsequent sources.
- If no configured source is present on the request, session creation falls back to cluster or global defaults.

### Common Proxy Configurations

#### oauth2-proxy
`oauth2-proxy` authenticates requests and passes identity in headers such as `X-Forwarded-Email` or `X-Forwarded-User`. While it sets a session cookie (`_oauth2_proxy`), the cookie value is encrypted and cannot be parsed directly by `harnx-serve`. Use the forwarded headers:

```bash
harnx-serve \
  --user-id-source "header:X-Forwarded-Email" \
  --user-id-source "header:X-Forwarded-User"
```

#### AWS Application Load Balancer (ALB) OIDC
AWS ALB authenticates callers via OIDC and injects `x-amzn-oidc-identity` (the user's identity/subject claim) and `x-amzn-oidc-data` (a signed JWT payload). Use the raw identity header `x-amzn-oidc-identity`, since `x-amzn-oidc-data` is an unparsed JWT:

```bash
harnx-serve \
  --user-id-source "header:x-amzn-oidc-identity"
```

#### AWS Application Load Balancer (ALB) mTLS

For ALB mTLS verify mode, use the client certificate subject header:

```bash
harnx-serve \
  --user-id-source "header:X-Amzn-Mtls-Clientcert-Subject"
```

`header:` and bare-name identity sources use the whole trimmed value of the first header field. Commas and backslashes remain intact: `X-Amzn-Mtls-Clientcert-Subject: CN=alice,OU=eng,O=Acme Corp` resolves to `CN=alice,OU=eng,O=Acme Corp`. Escaped commas, such as `CN=Smith\, Alice,OU=eng`, are preserved without DN parsing or unescaping. `header-csv:` explicitly selects the first list item instead. Both modes inspect only the first repeated field for user IDs; group/role headers collect all repeated fields.

### Security Warning

`harnx-serve` does not authenticate identity headers or cookies; it treats resolved values as opaque strings. The upstream proxy must authenticate callers and **replace, not append to**, client-supplied identity headers. Appending a trusted field leaves an untrusted first field in control; comma-joining values changes the identity rather than selecting the trusted value. Cookie sources must also contain only proxy-trusted identity values.

`user_id` is visible in session listings to callers permitted to view those sessions. When access rules are disabled, there is no per-user listing authorization. Use a non-secret account identifier, not access tokens, signed JWTs, or secret-bearing session cookies such as `_oauth2_proxy`. Values are stored as-is, not decrypted or verified; they remain in session metadata until the session is removed.

### Precedence and Immutability

When a session is first created:
1. Request identity (resolved from configured sources) takes highest precedence.
2. If absent, the destination cluster's configured `user_id` in `nats_servers/<cluster>.yaml` applies.
3. If absent, the global `user_id` from `config.yaml` or `HARNX_USER_ID` applies.
4. Nonblank explicit initializer properties (such as the A2A server's resolved user) and inherited identities take precedence over cluster and global defaults. Sub-agents and handoff-created sessions inherit the source session's `user_id`, unless its inheritance flag is disabled. If there is no inheritable nonblank identity, the destination defaults apply. Handoffs don't copy execution-context properties such as the source branch.

Blank explicit or inherited identity strings count as absent. Invalid request identity values still fail closed; they don't trigger default fallback.

Session user identity is stored once when the session metadata is created. It is immutable and never overwritten by subsequent prompts, handoffs into an existing session, or reconnecting callers. Concurrent creators use the identity of the first successful metadata creation. Promptless subscriptions and control commands don't create metadata; cancelling a never-prompted attached session is an idle no-op, and compacting it returns session not found.

### Group and Role Memberships

In addition to user identity, `harnx-serve` can extract group and role memberships from trusted HTTP request headers for use in access rules:
- Configured via `serve_group_headers` and `serve_role_headers` in `config.yaml`, the `HARNX_SERVE_GROUP_HEADERS` and `HARNX_SERVE_ROLE_HEADERS` environment variables (comma-separated header specs), or repeated `--group-header` and `--role-header` CLI flags. Non-empty CLI flags replace configuration file and environment settings.
- **Shared header specs**: `header:NAME` or bare `NAME` keeps each whole trimmed field as one opaque membership, without splitting commas or unquoting. `header-csv:NAME` enables quoted comma lists. Prefixes are case-insensitive. Unsupported source kinds (`cookie:`, `forge-fit`, `bearer-claim:`) and invalid names fail startup. By default, no membership headers are trusted.
- **List parsing (`header-csv:` only)**: All configured headers contribute values. Repeated fields of each name are comma-joined before parsing. Commas outside double quotes separate items, except backslash-escaped commas (`\,`), which stay literal. Unquoted items are trimmed; empty items (including `""`) are dropped. Order and duplicates are preserved.
- **Quoted items**: Double-quote an entire item to include DN commas. Inside quotes, `\"` becomes `"` and `\\` becomes `\`; other escapes stay literal, so RFC 4514 `\,` is preserved. Whitespace inside quotes is part of the value. After a closing quote, only whitespace and a comma or the end of the list are allowed.
- **Fail closed**: Invalid header bytes or malformed `header-csv:` lists (unterminated quotes or junk after a closing quote) reject the request with HTTP `401 Unauthorized` without echoing header contents.
- **Request-local authorization**: Memberships are evaluated on each request for access control and are not used for authorization persistence. Membership changes or revocations take effect on the next HTTP request. For prompt and tool tailoring, a snapshot of resolved caller principals (`user_id`, expanded `users`, `groups`, `roles`) is captured at session creation into the read-only `user_principals` session property (`__user__` in templates, `$user` in agent `patches`); access control never consults this snapshot. See [Customizing Agents by Caller](../../docs/configuration-guide.md#customizing-agents-by-caller-patches-and-__user__) and the [Agent Guide](../../docs/agent-guide.md#agent-patches-patches).
- **User ownership**: Groups and roles do not satisfy required user identification, do not act as user aliases, and never satisfy session ownership. Sessions are owned solely by immutable user IDs. Admin scope granted by group or role rules retains administrative access across all sessions.

**Migration:** Bare group/role names used to split comma lists. Change `X-Groups` to `header-csv:X-Groups` (and similarly for roles) wherever a proxy sends multiple memberships in one field. Use `header:X-Groups` for a single DN group with commas.

For example, configure `serve_group_headers: ["header-csv:X-Groups"]` or pass `--group-header header-csv:X-Groups`; this header supplies two DN groups and one plain group:

```http
X-Groups: "CN=admins,OU=eng,O=Acme", "CN=ops,O=Acme", plain-group
```

The group values are `CN=admins,OU=eng,O=Acme`, `CN=ops,O=Acme`, and `plain-group`. Access-rule `groups:` globs match each full value. The same list syntax applies to `header-csv:` user-id sources, which select only the first item; `header:` and bare names never unquote or split commas.

### User Aliases (`users.yaml`)

`harnx-serve` supports optional user alias configuration via `users.yaml` in the configuration directory (`~/.config/harnx/users.yaml`). See the [User Aliases section in the Configuration Guide](../../docs/configuration-guide.md#user-aliases-usersyaml) for schema, matching semantics, and examples.

- **Startup loading**: Loaded once at startup via `config_paths::local_path("users.yaml")`. Missing `users.yaml` preserves default singleton identity behavior. An invalid present file fails startup immediately with path context. Modifying the file requires a server restart; there is no live reload.
- **Caller authorization expansion**: An authenticated caller's identity expands into the first matching entry's `identities` list for agent access checks and session visibility under access rules. Overlapping entries do not merge (no transitive union).
- **Configured groups and roles**: Entries can declare optional `groups` and `roles`. When a caller matches an entry, its configured memberships are merged into the request memberships (appended after header values and deduped) for matching against `access.yaml` rules. Configured values apply even when no membership headers are configured.
- **Session ownership preserved**: Session creation (via `POST /v1/agents/{agent}/sessions` or a first prompt to `/v1/agents/{agent}/sessions/{session}`) stores the incoming caller's raw `user_id` in session metadata, never an alias entry name or other group identities. Configured groups and roles never grant session ownership. (For prompt and tool tailoring, resolved caller principals are snapshotted into the read-only `user_principals` property, but access control never uses that snapshot to grant ownership). Stored session owners are never expanded.
- **Behavior without access rules**: When `access.yaml` is absent, `harnx-serve` operates without access restrictions; `users.yaml` alone does not restrict endpoints or session access.
- **Security notice**: Because alias mappings expand caller permissions, `users.yaml` should be modified only by trusted operators.



## Access Control (`access.yaml`)

`harnx-serve` supports optional identity-based access control rules gating agent visibility, session access, and attachment downloads. See the comprehensive [Access Control section in the Configuration Guide](../../docs/configuration-guide.md#access-control-accessyaml) for full syntax and examples.

### Enabling Rules
- Place `access.yaml` in your harnx configuration directory (`~/.config/harnx/access.yaml`), or pass `--access-rules <PATH>` (env `HARNX_ACCESS_RULES`).
- When access rules are enabled, `harnx-serve` **requires** at least one trusted caller identity source via `--user-id-source` or `serve_user_id_sources` in `config.yaml`. Startup fails closed if rules are enabled without configured identity sources.
- When no rules file or flag is provided, access control is disabled and `harnx-serve` operates without authorization checks.

### Behavior Under Rules
- **Authentication**: Unauthenticated requests to `/v1/agents*` and `/v1/cid/*` return HTTP `401 Unauthorized`. Endpoints outside these paths (`/v1/models`, healthz, OPTIONS, and static web assets) remain open.
- **Agent Visibility**: Agents for which the caller has no scopes are hidden from `GET /v1/agents` and return HTTP `404 Not Found` across all sub-routes, identical to an unknown agent.
- **Session Scopes**:
  - `prompt`: Authorizes creating sessions on the agent and accessing sessions owned by the caller.
  - `admin`: Authorizes viewing, operating, compacting, and cancelling sessions owned by any user (and legacy unowned sessions). Does not grant creation permission.
  - Callers holding only `admin` scope receive HTTP `403 Forbidden` (`session creation requires prompt scope`) when attempting to create a session.
- **Client-Chosen Session IDs and First-Prompt Creation**: Callers with `prompt` scope can create a session with a client-chosen ID by sending a first prompt to `POST /v1/agents/{agent}/sessions/{session}`, or reserve a server-generated short ID via `POST /v1/agents/{agent}/sessions`:
  - **Qualifying first prompts**: For JSON-RPC (`Content-Type: application/json`), a `session/prompt` with nonblank `text` or non-empty `attachment_refs` and no `resume` entries (attachment-only prompts still need a `text` field, which may be `""`). Attachment ref checks run before reservation, so unauthorized, malformed, or `>32` `cid:media` refs (guard HTTP `404`) and preflight broker outages (HTTP `503`) leave the ID unclaimed. A broker outage during metadata reservation also returns HTTP `503` (including post-connect KV write timeouts, broken pipes, or no responders) without admitting the prompt, though it can occur after canonical KV creation committed before the ack arrived, leaving the ID claimed by the caller; retrying as the same caller resolves it as owned. For SSE (`Accept: text/event-stream`), a valid AG-UI run input with no `resume` decisions whose last supported message (after dropping unknown roles) is a nonblank `user` message.
  - **Scope check**: Callers without `prompt` scope (such as `admin`-only callers) receive HTTP `403 Forbidden` (`session creation requires prompt scope`) on a qualifying creating prompt to an unseen ID, with nothing created.
  - **Non-creating requests stay hidden**: All other requests on an unseen ID return HTTP `404 Not Found` like an inaccessible session for refused (404) outcomes; this parity doesn't cover every partial-outage outcome. No metadata, session actors, or transcript streams are created. This includes every non-prompt JSON-RPC method (`session/get`, `session/history`, `session/cancel`, `session/compact`, HITL, interaction, and surface-action RPCs), blank or `resume`-bearing `session/prompt` calls, promptless or `resume`-bearing SSE runs, malformed bodies, non-`POST` methods, and deeper subroutes (`/history`, `/events`, `/attachments`, `/metadata`). For `POST /v1/agents/{agent}/sessions/{session}` (with a claimable ID and no deeper segments), both unseen and foreign-owned IDs reject a declared `Content-Length` over 20 MiB immediately with the guard's `404` before reading any body bytes, and otherwise run the same bounded body read (20 MiB cap, 30-second deadline) before deciding; bodies over the cap or unfinished at the deadline return HTTP `404` either way (never `413`). Any ingress or reverse proxy in front of `harnx-serve` must allow request bodies of at least 20 MiB and at least a 30-second upload time for claim-candidate `POST` requests.
  - **Session ID format**: `:session` must be a safe path segment (non-empty, no `/`, `\`, `.`, or `..`) with no leading or trailing whitespace and satisfy the runtime's `SessionRef` format (`[A-Za-z0-9_-]+`, at least 6 characters, matching attachment uploads). Refused IDs — such as `demo`, `pr.12`, or whitespace-only/whitespace-padded IDs — receive the same opaque guard HTTP `404` without claiming or falling back to a generated ID.
  - **Race safety and orphan refusal**: Concurrent first prompts on the same ID race atomically in NATS KV (`SessionMetadataStore::create`). One creator wins and owns the session; competing callers are authorized against the winning record and get HTTP `404` if unauthorized. Creating prompts also return `404` without claiming the ID if a `SESSION_` transcript stream already exists without canonical metadata (orphan), or if the server already has a live session actor for that key with no canonical metadata on leader reread.
  - **Deletion and ID reuse**: Session deletion purges the canonical metadata record (`sessions/<storage_key>/meta`) last. If deletion fails mid-cascade, the canonical record remains with its old owner so other prompt-only callers continue to get `404` until `harnx delete session` is re-run or retention GC collects it on a later pass. If the final purge is skipped because the record revision changed, `harnx delete session` prints `Incomplete deletion of remote session` to stderr and exits `1`. Retrying deletion also rewrites the invocation journal's one-hour tombstone, which refuses journaled tool calls on the reused ID for another hour (it does not block claiming the ID). Once fully deleted, anyone with `prompt` scope can reuse the ID.
  - **Limitations**:
    - For refused `404` outcomes, the final response (status, body, and application headers) is identical between unseen and foreign IDs, and both run the same bounded body read; this parity doesn't cover every partial-outage outcome. Processing time can still differ slightly for callers with `prompt` scope because only the unseen-ID path parses the buffered body and checks `cid:media` refs. This is an accepted limitation because `prompt`-scope callers already learn whether an ID exists from create versus `404`.
    - Deletions that failed before canonical-last ordering may have left media or plans without a canonical metadata record. After upgrading, another user could claim that ID and read those leftovers through `/v1/cid/*`, and a later retry could delete the new claimant's session. Operators should finish retrying any such incomplete deletions with `harnx delete session <session_id> --agent <agent> --cluster <cluster>` before enabling or upgrading to a `harnx-serve` with client-chosen claims.
    - A client still holding an SSE stream on another `harnx-serve` process during an operator delete can observe events from a later claimant of the same ID, because actors are not incarnation-aware.
    - A worker still running at deletion time can write into a later claimant's session.
    - Reusing an ID within an hour of deletion can make journaled tool calls fail because the invocation journal retains a one-hour deletion tombstone, and retrying deletion rewrites that tombstone for another hour.
    - A prompt sent while deletion is already in flight keeps the existing race and may append to a session being torn down.
  - **Upgrade order**: On clusters that use access rules, upgrade every component that deletes sessions (`harnx` CLI, and `harnx-worker` instances running retention or orphan GC) and let any in-flight old cleanup runs finish **before** upgrading `harnx-serve`. Older deleters purge the canonical metadata record first and can leave new claimable partial deletions. When rolling back, roll back `harnx-serve` **before** rolling back the deleters. There is no feature flag.
- **Session Listings**: `GET /v1/agents/{agent}/sessions` filters results before pagination, showing only sessions the caller is authorized to access.
- **Prompt Attachment Refs**: On existing, accessible sessions, `cid:media:` refs supplied to the JSON-RPC `session/prompt` are authorized against the session that uploaded them before the prompt is admitted — the caller needs the same access to that source session that the session routes require (matching owner, alias expansion, or admin scope on the source agent, normalized for the destination cluster). At most 32 `cid:media` refs are accepted per prompt (HTTP 400 / `-32602` over the cap). An unauthorized, unattributed, or otherwise unreadable source returns the generic `-32005` `attachment not found` (HTTP 404) for the whole list, before any log append or turn activation; only confirmed broker outage during the check returns `-32003` (HTTP 503). Prompts without `cid:media` refs do no extra broker work. For creating prompts on unseen session IDs, pre-reservation attachment validation maps unauthorized, malformed, and `>32` `cid:media` refs to the guard's HTTP `404 Not Found` (not `400` / `-32602` or `-32005`), and broker outage to HTTP `503`.
- **CID Attachments**: `/v1/cid/*` validates caller access against the owning session embedded in the CID. Protected responses emit `Cache-Control: private, no-store`. Shared proxy caches should be purged when enabling rules.
## Quickstart

1. **Start server:**
   ```sh
   harnx-serve --addr 127.0.0.1:8000
   ```

2. **Subscribe to a session (SSE):**
   ```bash
   curl -N -X POST http://127.0.0.1:8000/v1/agents/my-agent/sessions/my-session \
     -H "Accept: text/event-stream" \
     -H "Content-Type: application/json" \
     -d '{"messages":[]}'
   ```

3. **Send a prompt via JSON-RPC:**
   ```bash
   curl -X POST http://127.0.0.1:8000/v1/agents/my-agent/sessions/my-session \
     -H "Content-Type: application/json" \
     -d '{
       "jsonrpc": "2.0",
       "id": 1,
       "method": "session/prompt",
       "params": { "text": "Hello, agent!" }
     }'
   ```

### Roadmap

- **Phase 1:** Text streaming, content negotiation, session enumeration, and history. (Done)
- **Phase 2:** Two-plane model (SSE + JSON-RPC), multi-subscriber broadcast, session cancellation. (Current)
- **NATS sessions:** Shared state and durable session persistence are NATS-backed.
