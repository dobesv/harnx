# harnx-serve

`harnx-serve` is a standalone HTTP server binary for the `harnx` agent harness. It provides a headless deployment option that serves the same HTTP API as `harnx --serve` but with a smaller dependency footprint, omitting the TUI and terminal-related components.

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

`harnx-serve` implements the AG-UI protocol, providing a content-negotiated, permalinkable REST surface under `/v1/agents`. This API is additive; the standard OpenAI-compatible endpoints (`/v1/chat/completions`, etc.) remain unaffected.

The AG-UI surface allows modern web interfaces (e.g., [assistant-ui](https://assistant-ui.com)) to interact with `harnx` agents using a stock `HttpAgent`.

### Endpoints

| Method | Path | Accept | Purpose |
| :--- | :--- | :--- | :--- |
| `GET` | `/v1/agents` | `application/json` | List all configured agents. |
| `GET` | `/v1/agents/:agent` | `text/html` | HTML placeholder page for the agent. |
| `GET` | `/v1/agents/:agent` | `application/json` | Agent details (name, description, active sessions). |
| `GET` | `/v1/agents/:agent/sessions` | `application/json` | List sessions for the agent (filtered by `agent_name`). |
| `GET` | `/v1/agents/:agent/sessions/:session` | `text/html` | HTML placeholder page for the session. |
| `GET` | `/v1/agents/:agent/sessions/:session` | `application/json` | Session history in AG-UI format (array of `{ id, role, content }`). |
| `POST` | `/v1/agents/:agent/sessions/:session` | `text/event-stream` | Execute an agent run via SSE. |

### Key Behaviors

- **Content Negotiation:** The same URL serves HTML (for browsers), JSON (for data), or SSE (for execution) based on the `Accept` header and HTTP method.
- **Permalink Model:** The `:session` ID in the URL corresponds to the AG-UI `threadId`. Sessions are lazily created upon the first run to a new session ID.
- **SSE Framing:** Each event is delivered as `data: {json}\n\n`. The event type is specified in the JSON `type` field.
- **Server-Authoritative History:** While clients send the full message array on each run, the server reconciles history and persists only new user turns. Resuming a session continues context without duplicating messages.
- **Single-Message Run Contract:** Phase 1 accepts exactly **one** new user message per run. 
  - If the sent array contains NO new messages (exact resend) → returns **400 Bad Request**.
  - If it contains MORE THAN ONE new message → returns **400 Bad Request**.
  - This matches standard `assistant-ui` `HttpAgent` behavior (full history + one new turn).

#### P1 Event Set
The following events are supported in the initial implementation:
1. `RUN_STARTED`
2. `TEXT_MESSAGE_START`
3. `TEXT_MESSAGE_CONTENT` (streamed)
4. `TEXT_MESSAGE_END`
5. `RUN_FINISHED` (Successful completion)
6. `RUN_ERROR` (Terminal failure; replaces `RUN_FINISHED`). On error, the stream terminates immediately without emitting `TEXT_MESSAGE_END` or `RUN_FINISHED`. Clients should treat `RUN_ERROR` as the end of the run and the current message.

### Operational Notes

- **Persistence and `--dry-run`:** Session persistence is skipped in `--dry-run` mode. To enable session enumeration, history, and resumption, run the server without the `--dry-run` flag.
- **Consistency Barrier:** History and enumeration reflect a run only AFTER the SSE stream reaches `RUN_FINISHED`. The turn is persisted as it completes.

### Quickstart

1. **Start the server:**
   ```sh
   harnx-serve --addr 127.0.0.1:8000
   ```
   *Note: Do not use `--dry-run` if you want sessions to persist.*

2. **Execute a run via `curl`:**
   ```bash
   curl -N -X POST http://127.0.0.1:8000/v1/agents/my-agent/sessions/my-session \
     -H "Accept: text/event-stream" \
     -H "Content-Type: application/json" \
     -d '{
       "threadId": "my-session",
       "messages": [
         { "role": "user", "content": "hello" }
       ]
     }'
   ```

3. **Expected SSE Output:**
   ```text
   data: {"type":"RUN_STARTED","threadId":"my-session","runId":"..."}

   data: {"type":"TEXT_MESSAGE_START","messageId":"<uuid>","role":"assistant"}

   data: {"type":"TEXT_MESSAGE_CONTENT","messageId":"<uuid>","delta":"Hello"}

   data: {"type":"TEXT_MESSAGE_END","messageId":"<uuid>"}

   data: {"type":"RUN_FINISHED","threadId":"my-session","runId":"..."}
   ```

   The assistant `messageId` in the SSE stream is a durable UUID. This `messageId` 
   matches the `id` returned by the history endpoint for that message and is 
   stable across session reloads and history compaction (enabling permalinks).

4. **Configure an AG-UI `HttpAgent` (JS):**
   ```javascript
   const agent = new HttpAgent({
     url: "http://localhost:8000/v1/agents/my-agent/sessions/my-session"
   });
   ```

### Roadmap

The following features are included in this release (Phase 1 + 1.5):
- **P1:** Text streaming, content negotiation, session enumeration, and history.
- **P1.5:** Durable per-message UUIDs (stable message IDs across compaction/reloads).

The following features are planned for future phases:
- **P2:** Tool and reasoning events.
- **P3:** NATS-backed sessions and shared state.
- **P4:** Full `assistant-ui` frontend integration (replacing HTML placeholders).
