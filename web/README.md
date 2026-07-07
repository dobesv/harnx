# Harnx Web Chat Client

This is the first-version React + Vite + TypeScript web chat client for the `harnx-serve` AG-UI server, as tracked in issue [#959](https://github.com/ai-tools/harnx-ag-ui/issues/959).

## Prerequisites

- **Node.js**: ≥ 24.12.0 (pinned in `web/.nvmrc`)
- **pnpm**: ≥ 11.10.0 (pinned in `packageManager` field)

This is a standalone project within the `web/` directory. Rust developers do not need to modify their environment to run the frontend.

## Local Development Loop

Developing the chat client requires a two-terminal setup: one for the backend server and one for the frontend dev server.

### Terminal 1: Backend (harnx-serve)

Run the AG-UI server in dry-run mode. This serves the API in "echo mode" (no LLM or persistence required), which is sufficient to verify the connection.

```bash
cargo run --bin harnx-serve -- --dry-run
```

- **Server URL**: http://127.0.0.1:8000
- **Verification**: `GET http://127.0.0.1:8000/v1/agents` should return at least one agent.

### Terminal 2: Frontend (Vite)

Install dependencies and start the Vite development server.

```bash
cd web
pnpm install
pnpm dev
```

- **Vite URL**: http://localhost:5173

### Vite Dev Proxy

The development server is configured in `web/vite.config.ts` to proxy requests starting with `/v1` to `http://127.0.0.1:8000`. This allows the client to use same-origin relative URLs (e.g., `/v1/chat/completions`) and bypasses CORS during development.

The proxy is configured to be **SSE-safe**. It disables buffering for `text/event-stream` responses, ensuring that assistant streaming (thinking and tool calls) passes through to the browser unbuffered.

## How to Use

1. Open http://localhost:5173 in your browser.
2. Select an agent from the list.
3. Click **"New chat"**.
4. Type a message and press Enter.
5. Watch the streamed assistant reply. Text, thinking, and tool calls will render as the agent emits them.
6. Use the **Cancel** button to stop a running turn.

## Feature Scope (v1)

- **Included**:
  - Streaming text, thinking, and tool-call rendering.
  - Session switching.
  - Turn cancellation.
- **Out of Scope** (Deferred to `harnx-webui-parity` backlog):
  - File attachments.
  - Tool approval workflows.
  - Message editing and rewinding.
  - Model switching.

### Known Cosmetic Issues

- **C5**: When a user sends a message, there may be a brief flicker when the optimistic local message is reconciled with the `MESSAGES_SNAPSHOT` from the server. This is an accepted behavior for the first version.

## Build and CI

### Production Build
To generate a production build in `web/dist/`:
```bash
pnpm build
```

### Type Checking
To run the TypeScript compiler without emitting files:
```bash
pnpm exec tsc --noEmit
```

### CI Workflow
The web client is integrated into the `.github/workflows/web-ci.yml` lane. It runs automatically on any changes to the `web/**` path.

## Future: Release Embedding

Single-binary embedding via `rust-embed` (embedding the `web/dist` assets into the `harnx-serve` binary) is deferred and does not gate the v1 release. See the `harnx-webui-parity` backlog for details. Currently, the web client is served exclusively via the Vite dev server or a standalone static host.
