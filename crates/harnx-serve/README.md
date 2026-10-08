# harnx-serve

`harnx-serve` is standalone HTTP server binary for `harnx` agent harness. It is now the only supported way to run server mode, with a smaller dependency footprint that omits TUI and terminal-related components.

## Overview

The server allows external clients (such as IDE plugins or web interfaces) to interact with `harnx` agents over HTTP. It supports agent execution, session management, and MCP tool orchestration.

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
| `--user-id-source <SOURCE>` | | Identity source (`header:NAME`, `cookie:NAME`, or bare `NAME`); repeat in priority order. Replaces `serve_user_id_sources` from config. |
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
The `:session` path segment is the canonical short Harnx ID used by URLs, the
NATS stream, and canonical session metadata. AG-UI's `ThreadId` type requires a UUID, so
`ag_ui::derive_thread_id` deterministically maps a short ID to UUID v5 at the
wire boundary (and passes legacy UUID session IDs through). The derived UUID is
never used as a persistence or routing key.

### Endpoints

| Method | Path | Accept / Content-Type | Purpose |
| :--- | :--- | :--- | :--- |
| `GET` | `/v1/agents` | `application/json` | List all configured agents. |
| `GET` | `/v1/agents/:agent` | `application/json` | Agent details (name, description, active sessions). |
| `GET` | `/v1/agents/:agent/sessions` | `application/json` | List sessions for the agent. |
| `POST` | `/v1/agents/:agent/sessions` | `application/json` | Reserve a canonical short session ID. |
| `GET` | `/v1/agents/:agent/sessions/:session` | `application/json` | Session history in AG-UI format. |
| `GET` | `/v1/agents/:agent/sessions/:session/metadata` | `application/json` | Read redacted canonical session metadata. |
| `PATCH` | `/v1/agents/:agent/sessions/:session/metadata` | `application/json` | Update title, variables, or explicit session overrides. |
| `PUT` | `/v1/agents/:agent/sessions/:session/metadata/extensions/:namespace` | `application/json` | Atomically replace one extension namespace. |
| `DELETE` | `/v1/agents/:agent/sessions/:session/metadata/extensions/:namespace` | `application/json` | Delete one extension namespace. |
| `GET` | `/v1/agents/:agent/sessions/:session/events` | `Accept: text/event-stream` | Notify passive clients when any frontend updates the session. |
| `POST` | `/v1/agents/:agent/sessions/:session` | `Accept: text/event-stream` | **Subscription Plane**: SSE event stream. |
| `POST` | `/v1/agents/:agent/sessions/:session` | `Content-Type: application/json` | **Control Plane**: JSON-RPC 2.0 interface. |
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

Provides AG-UI events for a run. The body's **last message** selects the mode:

- **Prompted run** (last message is a non-empty `user` message whose ID is not
  already present in the authoritative snapshot): a pure delta
  stream — `RUN_STARTED` → `session_attach_boundary` →
  `STEP_*`/`TEXT_MESSAGE_*`/`THINKING_*`/`TOOL_CALL_*`/`CUSTOM` → `RUN_FINISHED`
  (or `RUN_ERROR`). The stream **terminates** after the
  terminal event so the client's `runAgent()` promise resolves. No
  `MESSAGES_SNAPSHOT` is emitted (it would predate the just-sent user message).
- **Promptless join** (no non-empty trailing user message): hydrates with a
  synthetic `RUN_STARTED` → `session_attach_boundary` → `MESSAGES_SNAPSHOT`.
  For an idle session it appends
  a synthetic `RUN_FINISHED` and closes; for a running or interrupted session it
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
  **Result:** `{ "state": { … }, "history_snapshot": [...], "history_warnings": [], "capabilities": { "multiClient": true, "persistence": "nats" } }`

  `state` is one of five shapes, derived from the durable session log (plus the
  session lease, so a turn this server never started is still reported as
  running):
  - `{ "status": "idle" }`
  - `{ "status": "running", "run_id": "…", "started_at": "…" }` — a turn owned
    by a remote worker, or an admitted prompt waiting for its worker, reports
    plain `{ "status": "running" }`, with no run id. This means durable work is
    outstanding, not that a worker necessarily holds the lease.
  - `{ "status": "interrupting" }` — this server's interrupt append is in
    flight.
  - `{ "status": "interrupted", "cancel_seq": 12 }` — a `Cancel` at that log
    sequence ended the turn.
  - `{ "status": "awaiting_approval", "pending_interrupts": { … } }` — the turn
    is parked at an approval gate. It is not interrupted; the client's next move
    is a decision, not another cancel.
- **`session/prompt`**: Sends a new user prompt, creating the session if it doesn't exist.
  ```json
  { "jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": { "text": "hello" } }
  ```
  **Result:** `{ "status": "accepted", "run_id": "..." }` (idle) or `{ "status": "enqueued", "run_id": "..." }` (running).
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
- `-32001`: Unknown session (HTTP 404) for operations that require an existing session, such as `session/get`, `session/cancel`, `session/compact`, and read-state updates. `session/prompt` creates the session if absent.
- `-32003`: Session actor unreachable (HTTP 503). Transient: the session's in-process actor could not be reached, so retry the call.
- `-32601`: Method not found
- Standard JSON-RPC 2.0 codes (-32700, -32600, -32602)

`session/cancel` on an idle session is not an error; it returns
`{ "outcome": "idle" }`.

### Client Implementation Flow

1. **Create**: `POST` the session collection (`POST /v1/agents/{agent}/sessions`) and use the returned `session_id` as the URL, NATS stream, and persistence identity.
   - When access control rules are active, callers **must** reserve a session ID via this `POST` endpoint before prompting.
   - When access rules are disabled, clients that choose their own session IDs can skip this POST: send `session/prompt` to the chosen session URL to create the session and admit its first prompt.
2. **Connect**: Open the session event feed and use promptless AG-UI runs to hydrate or join an active run. An attach after prompt admission waits for a worker or durable completion even before the worker claims the lease. If no worker claims it within `nats_lease_acquisition_timeout_secs` (default 60 seconds), the stream closes with `RUN_ERROR`, not a false `RUN_FINISHED`. This observation timeout doesn't cancel the prompt. Restore the worker and attach again, or explicitly cancel the pending turn; unanswered durable input remains `running` until completed, failed, cancelled, or retracted.
3. **Drive**: Use JSON-RPC on the same canonical session URL to send prompts
   (`session/prompt`) and interrupt runs (`session/cancel`).
4. **Stateless UI**: Clients only send new inputs via RPC; they do not need to re-POST full transcript.

### Disconnect Semantics (D5)

SSE connections are decoupled from execution. Dropping an SSE connection (e.g., reloading page) **does not stop** a running agent. Run continues to completion and persists. Only `session/cancel` or terminal error stops a run.

### Divergence from AG-UI Standards

This implementation deliberately diverges from generic AG-UI/assistant-ui standards for optimization:
- **Last-Message Inspection:** Decision to start a run is based on last message in SSE POST body.
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


### AgentEvent → AG-UI mapping

| harnx `AgentEvent` | AG-UI event(s) | Notes |
| :--- | :--- | :--- |
| `Model::MessageChunk` / `Model::Final` | `TEXT_MESSAGE_CONTENT` | `TEXT_MESSAGE_START` / `TEXT_MESSAGE_END` still come from session actor lifecycle. |
| `Model::ThoughtChunk` | `THINKING_START`, `THINKING_TEXT_MESSAGE_START`, `THINKING_TEXT_MESSAGE_CONTENT`, `THINKING_TEXT_MESSAGE_END`, `THINKING_END` | Sink keeps per-run thinking state so multi-chunk reasoning stays one segment. |
| `Tool::*` | `TOOL_CALL_START`, `TOOL_CALL_ARGS`, `TOOL_CALL_END`, `TOOL_CALL_RESULT` | Progress/update still dropped as too noisy. |
| `Turn::Started` / `Turn::Ended` | `STEP_STARTED` / `STEP_FINISHED` | Step names use `turn-N`. |
| `Turn::SubAgentProgress` | `CUSTOM` | Name: `sub_agent_progress`. The invocation ID correlates reused child sessions, and `tool_call_id`, when present, names the parent tool call that started the child; progress carries running/done/failed status, elapsed milliseconds, direct token usage, direct tool-call count, and optional `title` (present when the sub-agent reported a session title). |
| `Turn::RetryAttempt` / `ModelFallback` / `HandoffRequested` | `CUSTOM` | Names: `turn_retry_attempt`, `turn_model_fallback`, `turn_handoff_requested`. A handoff request is informational and may carry no session ID; clients must not navigate on it. |
| `Session::HandoffCommitted` | `CUSTOM` | Name: `session_handoff`. Emitted only after the target prompt is accepted for dispatch, with nonempty `agent`, resolved `session_id`, optional `handoff_tool_call_id`, and optional durable `after_seq`. Hydration always sets `after_seq` from the handoff log entry's sequence. |
| Attach boundary | `CUSTOM` | Name: `session_attach_boundary`. Emitted immediately after `RUN_STARTED` with `attached_seq` set to the durable log tail observed at attach time, before snapshot, hydrated control, or live events. |
| `Session::Compacting*` | `CUSTOM` (+ `MESSAGES_SNAPSHOT` on completed) | Names: `session_compacting_started`, `session_compacting_completed`, `session_compacting_failed`. Completion re-snapshots transcript because compaction mutates history. |
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

### Tool Approval Interrupts

When a `PreToolUse` hook returns `{"permissionDecision": "ask"}`, the lease-holding
worker persists `HitlApprovalRequested`. The server derives `Interrupted` state
from this durable log, including after reconnects and server restarts. Decisions
are routed to the worker, which writes `HitlApprovalDecision` using lease fencing,
stream-tail compare-and-set, and ownership revalidation. Only a winning durable
decision can resume the tool round; the server does not retain a continuation.

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

## Request Identity

`harnx-serve` can associate incoming requests with an opaque user identity string, storing it in canonical session metadata (`user_id`).

### Configuration

Identity sources can be configured via `serve_user_id_sources` in `config.yaml`, the `HARNX_SERVE_USER_ID_SOURCES` environment variable, or repeated `--user-id-source` CLI flags. CLI flags replace configuration file sources when supplied.

Each source is an ordered entry:
- `header:NAME` — reads HTTP request header `NAME` (case-insensitive header name).
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

### Security Warning

`harnx-serve` does not authenticate identity headers or cookies; it treats resolved values as opaque strings. The upstream proxy must authenticate callers and **replace, not append to**, client-supplied identity headers. Header sources use the first comma-separated value, so appending a trusted identity after an untrusted value still permits spoofing. Cookie sources must also contain only proxy-trusted identity values.

`user_id` is visible in session listings to callers permitted to view those sessions. When access rules are disabled, there is no per-user listing authorization. Use a non-secret account identifier, not access tokens, signed JWTs, or secret-bearing session cookies such as `_oauth2_proxy`. Values are stored as-is, not decrypted or verified; they remain in session metadata until the session is removed.

### Precedence and Immutability

When a session is first created:
1. Request identity (resolved from configured sources) takes highest precedence.
2. If absent, the destination cluster's configured `user_id` in `nats_servers/<cluster>.yaml` applies.
3. If absent, the global `user_id` from `config.yaml` or `HARNX_USER_ID` applies.
4. Nonblank explicit initializer properties (such as the A2A server's resolved user) and inherited identities take precedence over cluster and global defaults. Sub-agents and handoff-created sessions inherit the source session's `user_id`, unless its inheritance flag is disabled. If there is no inheritable nonblank identity, the destination defaults apply. Handoffs don't copy execution-context properties such as the source branch.

Blank explicit or inherited identity strings count as absent. Invalid request identity values still fail closed; they don't trigger default fallback.

Session user identity is stored once when the session metadata is created. It is immutable and never overwritten by subsequent prompts, handoffs into an existing session, or reconnecting callers. Concurrent creators use the identity of the first successful metadata creation. Promptless subscriptions and control commands don't create metadata; cancelling a never-prompted attached session is an idle no-op, and compacting it returns session not found.


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
- **Pre-allocation Requirement**: Callers must reserve a session ID via `POST /v1/agents/{agent}/sessions` before prompting. Implicit session creation on unreserved session IDs returns HTTP `404 Not Found`.
- **Session Listings**: `GET /v1/agents/{agent}/sessions` filters results before pagination, showing only sessions the caller is authorized to access.
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
