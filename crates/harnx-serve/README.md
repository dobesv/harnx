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
| `--mcp-root <PATH>` | | Add one or more MCP roots (comma-separated). |

## AG-UI (Agent User Interaction Protocol)

`harnx-serve` implements the AG-UI protocol, providing a content-negotiated, permalinkable REST surface under `/v1/agents`. AG-UI is the sole interactive surface.

The AG-UI surface allows modern web interfaces to interact with `harnx` agents using a real-time event stream and a JSON-RPC control plane.

### Endpoints

| Method | Path | Accept / Content-Type | Purpose |
| :--- | :--- | :--- | :--- |
| `GET` | `/v1/agents` | `application/json` | List all configured agents. |
| `GET` | `/v1/agents/:agent` | `application/json` | Agent details (name, description, active sessions). |
| `GET` | `/v1/agents/:agent/sessions` | `application/json` | List sessions for the agent. |
| `GET` | `/v1/agents/:agent/sessions/:session` | `application/json` | Session history in AG-UI format. |
| `POST` | `/v1/agents/:agent/sessions/:session` | `text/event-stream` | **Subscription Plane**: SSE event stream. |
| `POST` | `/v1/agents/:agent/sessions/:session/rpc`| `application/json` | **Control Plane**: JSON-RPC 2.0 interface. |

## AG-UI Support (Phase 2)

`harnx-serve` implements a tailored two-plane communication model. This model provides real-time event streaming and a JSON-RPC control plane for managing agent sessions.

### Two-Plane Architecture

#### 1. SSE Subscription Plane
**Endpoint:** `POST /v1/agents/:agent/sessions/:session`  
**Header:** `Accept: text/event-stream`

Provides a live stream of AG-UI events. Multiple concurrent subscribers are supported via broadcast.

- **Initialization:** Emits a `MESSAGES_SNAPSHOT` event immediately upon connection with the current history.
- **Run Triggering:** Inspects the **last message** of the JSON body.
  - If the last message is a `user` message → starts/continues a run.
  - Otherwise (or empty body) → joins/resumes the session without starting a new run.
- **Keep-Alive:** Sends `: keep-alive` SSE comments approximately every 15 seconds.
- **Events:** Streams `RUN_STARTED`, `STEP_STARTED`, `TEXT_MESSAGE_START`, `THINKING_*`, `TEXT_MESSAGE_CONTENT`, `TOOL_CALL_*`, `CUSTOM`, `STEP_FINISHED`, `RUN_FINISHED`, and `RUN_ERROR`.

#### 2. JSON-RPC 2.0 Control Plane
**Endpoint:** `POST /v1/agents/:agent/sessions/:session/rpc`  
**Header:** `Content-Type: application/json`

Sibling endpoint for programmatic control.

- **`session/get`**: Returns session state and capabilities.
  ```json
  { "jsonrpc": "2.0", "id": 1, "method": "session/get" }
  ```
  **Result:** `{ "state": { "status": "idle" }, "history_snapshot": [...], "capabilities": { "multiClient": true, "persistence": "filesystem" } }` (a running session reports `"state": { "status": "running", "run_id": "…", "started_at": "…" }`)
- **`session/prompt`**: Sends a new user prompt.
  ```json
  { "jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": { "text": "hello" } }
  ```
  **Result:** `{ "status": "accepted", "run_id": "..." }` (idle) or `{ "status": "enqueued", "run_id": "..." }` (running).
- **`session/cancel`**: Aborts the running agent loop.
  ```json
  { "jsonrpc": "2.0", "id": 3, "method": "session/cancel" }
  ```
  **Result:** `{ "cancelled": true }`

**Error Codes:**
- `-32001`: Unknown session
- `-32601`: Method not found
- Standard JSON-RPC 2.0 codes (-32700, -32600, -32602)

### Client Implementation Flow

1. **Connect**: Open an SSE connection to receive history and subscribe to live events.
2. **Drive**: Use the JSON-RPC endpoint to send prompts (`session/prompt`) or cancel runs (`session/cancel`).
3. **Stateless UI**: Clients only send new inputs via RPC; they do not need to re-POST the full transcript.

### Disconnect Semantics (D5)

SSE connections are decoupled from execution. Dropping an SSE connection (e.g., reloading the page) **does not stop** a running agent. The run continues to completion and persists. Only `session/cancel` or a terminal error stops a run.

### Divergence from AG-UI Standards

This implementation deliberately diverges from generic AG-UI/assistant-ui standards for optimization:
- **Last-Message Inspection:** Decision to start a run is based on the last message in the SSE POST body.
- **Two-Plane Control:** Uses JSON-RPC instead of standard RESTful run endpoints to support mid-run injection.
- **No Staleness Guards:** Omits request reconciliation and version/sequence guards.

### Phase B Scope

Cross-process live synchronization via NATS and distributed persistence (Issue #848) are deferred to Phase B.


### AgentEvent → AG-UI mapping

| harnx `AgentEvent` | AG-UI event(s) | Notes |
| :--- | :--- | :--- |
| `Model::MessageChunk` / `Model::Final` | `TEXT_MESSAGE_CONTENT` | `TEXT_MESSAGE_START` / `TEXT_MESSAGE_END` still come from session actor lifecycle. |
| `Model::ThoughtChunk` | `THINKING_START`, `THINKING_TEXT_MESSAGE_START`, `THINKING_TEXT_MESSAGE_CONTENT`, `THINKING_TEXT_MESSAGE_END`, `THINKING_END` | Sink keeps per-run thinking state so multi-chunk reasoning stays one segment. |
| `Tool::*` | `TOOL_CALL_START`, `TOOL_CALL_ARGS`, `TOOL_CALL_END`, `TOOL_CALL_RESULT` | Progress/update still dropped as too noisy. |
| `Turn::Started` / `Turn::Ended` | `STEP_STARTED` / `STEP_FINISHED` | Step names use `turn-N`. |
| `Turn::RetryAttempt` / `ModelFallback` / `HandoffRequested` | `CUSTOM` | Names: `turn_retry_attempt`, `turn_model_fallback`, `turn_handoff_requested`. |
| `Session::Compacting*` | `CUSTOM` (+ `MESSAGES_SNAPSHOT` on completed) | Names: `session_compacting_started`, `session_compacting_completed`, `session_compacting_failed`. Completion re-snapshots transcript because compaction mutates history. |
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

- **Persistence and `--dry-run`:** Session persistence is skipped in `--dry-run` mode.
- **Consistency Barrier:** History reflects a turn only AFTER it completes and is persisted.

## Quickstart

1. **Start the server:**
   ```sh
   harnx-serve --addr 127.0.0.1:8000
   ```

2. **Subscribe to a session (SSE):**
   ```bash
   curl -N -X POST http://127.0.0.1:8000/v1/agents/my-agent/sessions/my-session \
     -H "Accept: text/event-stream" \
     -d '{}'
   ```

3. **Send a prompt via JSON-RPC:**
   ```bash
   curl -X POST http://127.0.0.1:8000/v1/agents/my-agent/sessions/my-session/rpc \
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
- **Phase B:** NATS-backed sessions, shared state, and distributed persistence. (Planned)
