# harnx-serve

`harnx-serve` is standalone HTTP server binary for `harnx` agent harness. It is now the only supported way to run server mode, with a smaller dependency footprint that omits TUI and terminal-related components.

## Overview

The server allows external clients (such as IDE plugins or web interfaces) to interact with `harnx` agents over HTTP. It supports agent execution, session management, and MCP tool orchestration.

## Installation

To install `harnx-serve` from the `harnx` workspace:

```sh
cargo install --path crates/harnx-serve
```

## CLI Options

| Option | Short | Description |
| :--- | :--- | :--- |
| `--addr <ADDRESS>` | `-a` | Listen address (default from `config.yaml` or `127.0.0.1:8000`). |
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
  stream — `RUN_STARTED` → `STEP_*`/`TEXT_MESSAGE_*`/`THINKING_*`/`TOOL_CALL_*`/
  `CUSTOM` → `RUN_FINISHED` (or `RUN_ERROR`). The stream **terminates** after the
  terminal event so the client's `runAgent()` promise resolves. No
  `MESSAGES_SNAPSHOT` is emitted (it would predate the just-sent user message).
- **Promptless join** (no non-empty trailing user message): hydrates with a
  synthetic `RUN_STARTED` → `MESSAGES_SNAPSHOT`. For an idle session it appends
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
  **Result:** `{ "state": { "status": "idle" }, "history_snapshot": [...], "history_warnings": [], "capabilities": { "multiClient": true, "persistence": "nats" } }` (a running session reports `"state": { "status": "running", "run_id": "…", "started_at": "…" }`)
- **`session/prompt`**: Sends a new user prompt.
  ```json
  { "jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": { "text": "hello" } }
  ```
  **Result:** `{ "status": "accepted", "run_id": "..." }` (idle) or `{ "status": "enqueued", "run_id": "..." }` (running).
- **`session/cancel`**: Aborts running agent loop.
  ```json
  { "jsonrpc": "2.0", "id": 3, "method": "session/cancel" }
  ```
  **Result:** `{ "cancelled": true }`

**Error Codes:**
- `-32001`: Unknown session (HTTP 404)
- `-32002`: Session is not running — `session/cancel` on an idle session (HTTP 400)
- `-32003`: Session actor unreachable (HTTP 503). Transient: the session's in-process actor could not be reached, so retry the call.
- `-32601`: Method not found
- Standard JSON-RPC 2.0 codes (-32700, -32600, -32602)

### Client Implementation Flow

1. **Create**: `POST` the session collection and use the returned `session_id` as the URL, NATS stream, and persistence identity.
2. **Connect**: Open the session event feed and use promptless AG-UI runs to hydrate or join an active run.
3. **Drive**: Use JSON-RPC on same canonical session URL to send prompts (`session/prompt`) or cancel runs (`session/cancel`).
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

### Phase B Scope

Cross-process live synchronization and durable session persistence use NATS.


### AgentEvent → AG-UI mapping

| harnx `AgentEvent` | AG-UI event(s) | Notes |
| :--- | :--- | :--- |
| `Model::MessageChunk` / `Model::Final` | `TEXT_MESSAGE_CONTENT` | `TEXT_MESSAGE_START` / `TEXT_MESSAGE_END` still come from session actor lifecycle. |
| `Model::ThoughtChunk` | `THINKING_START`, `THINKING_TEXT_MESSAGE_START`, `THINKING_TEXT_MESSAGE_CONTENT`, `THINKING_TEXT_MESSAGE_END`, `THINKING_END` | Sink keeps per-run thinking state so multi-chunk reasoning stays one segment. |
| `Tool::*` | `TOOL_CALL_START`, `TOOL_CALL_ARGS`, `TOOL_CALL_END`, `TOOL_CALL_RESULT` | Progress/update still dropped as too noisy. |
| `Turn::Started` / `Turn::Ended` | `STEP_STARTED` / `STEP_FINISHED` | Step names use `turn-N`. |
| `Turn::RetryAttempt` / `ModelFallback` / `HandoffRequested` | `CUSTOM` | Names: `turn_retry_attempt`, `turn_model_fallback`, `turn_handoff_requested`. |
| `Session::Compacting*` | `CUSTOM` (+ `MESSAGES_SNAPSHOT` on completed) | Names: `session_compacting_started`, `session_compacting_completed`, `session_compacting_failed`. Completion re-snapshots transcript because compaction mutates history. |
| `Session::TitleUpdated` / `TitleGenerationFailed` | `CUSTOM` | Names: `session_title_updated`, `session_title_generation_failed`. |
| `Session::Saved` / `AgentInitializing` / `ModelChanged` / `RagIndexing` / `Generic` | `CUSTOM` | Stable names prefixed with `session_...`. |
| `Session::LogSeqAssigned` | dropped | Persistence bookkeeping for local transcript patching; not useful on AG-UI wire. |
| `Plan { entries }` | `CUSTOM` | Name: `plan`. Carries serialized plan entries for plan/todo panels. |
| `Status(StatusLine)` | dropped | Spinner/status chatter is high-frequency and not durable transcript structure, so server keeps it off wire. |
| `Model::Usage` | `CUSTOM` | Name: `usage`. Carries input/output/cached/session label for token-cost displays. |
| `Notice::Error` / `Model::Error` | `RUN_ERROR` | Terminal user-visible error path. |

Intentionally dropped today:
- `Tool::Progress` / `Tool::Update` — high-volume progress noise; clients still get durable start/result framing.
- `Status(StatusLine)` — spinner/status chatter is frequent and not durable transcript structure.
- `Notice::Info` / `Notice::Warning` — not currently emitted in server flows worth surfacing; omitted to avoid custom-event spam.
## Operational Notes

- **Persistence and `--dry-run`:** NATS transcript writes are skipped in `--dry-run` mode. A generated session ID is still reserved with canonical NATS session metadata.
- **Durable Refresh:** History reflects the latest durably appended log entries.
  Live notifications are advisory; clients reload the authoritative transcript
  after each notification and on reconnect.

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
