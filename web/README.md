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
  - Tool approval and session handoff confirmations (approve/deny responses and reconnect replay).
- **Out of Scope** (Deferred to `harnx-webui-parity` backlog):
  - General transcript message editing and history rewinding.
  - File attachments.
  - Model switching.
  - NATS single-owner-route fanout redesign for multi-client tool approvals.

### Known Cosmetic Issues

- **C5**: There is no optimistic rendering for out-of-band user messages. When sending to an existing session, the composer disables and shows a spinner until the backend hydrates the message into the transcript.

## Build and CI

### Production Build
To generate a production build in `web/dist/`:
```bash
pnpm build
```

### Type Checking
This project uses TypeScript project references. The root `tsconfig.json` has `"files": []` and only references sub-projects. Run `-b` (build mode) to typecheck all files:
```bash
pnpm exec tsc -b
```
A bare `tsc --noEmit` loads the root config and typechecks **zero files** — it always exits 0 and provides no verification. This is a common footgun: always use `tsc -b`.

### CI Workflow
The web client is integrated into the `.github/workflows/web-ci.yml` lane. It runs automatically on any changes to the `web/**` path.

## Future: Release Embedding

Single-binary embedding via `rust-embed` (embedding the `web/dist` assets into the `harnx-serve` binary) is deferred and does not gate the v1 release. See the `harnx-webui-parity` backlog for details. Currently, the web client is served exclusively via the Vite dev server or a standalone static host.

## HTTP Layer Architecture

HTTP calls span two modules, not one: `web/src/api.ts` and `web/src/cancellationApi.ts` (added in PR #1817). Shared primitives live in `web/src/httpClient.ts` to avoid circular imports.

### Retry Behavior

- **Reads auto-retry:** `listAgents`, `listSessions`, `getAgent` use `fetchJsonWithRetry` with capped exponential backoff (initial ~0.5–1.5s, steady-state mean 60s) and jitter. Backend outages show a connecting state during initial load or a non-blocking banner for background failures.
- **Writes do not replay:** `createSession`, `uploadAttachment`, `sendPrompt`, `submitHitlDecision`, and observe-only calls (`sessionControl`, `cancel`) use `observedFetch` — they notify connection status but never auto-retry. The caller (or polling interval) manages retry timing.

### Status Classification Order

**Status classification happens BEFORE body parsing.** This is critical:

- HTTP 5xx responses (including 502/504 from proxy or ingress with HTML bodies) are classified as `TransientError` and enter the retry loop.
- HTTP 4xx responses are classified as `PermanentError` and fail immediately.
- Malformed JSON on 2xx is a `PermanentError`.

A 502 Bad Gateway with an HTML error page must retry, not fail permanently as "malformed JSON."

### Cancel Semantics (from PR #1817)

`cancel()` in `cancellationApi.ts` preserves its original contract:
- Parses JSON body before checking `res.ok`, so a JSON-RPC error (e.g., `-32002` idle session) can be inspected.
- Treats `-32002` as benign success (session already idle).
- Uses a 2-second `AbortSignal.timeout`.

### DEV Test Seam

In development mode (`import.meta.env.DEV`), `window.__harnxConnection = { initialDelayMs, maxDelayMs }` compresses backoff delays for E2E tests. Production ignores this override. Tests set this via `page.addInitScript()` before page load.

### Connection Coordinator

`web/src/connection.ts` is a React-free module-level singleton implementing the retry round scheduler with frozen snapshots. React integration uses `useSyncExternalStore` in `web/src/useConnectionStatus.ts`. Do not import React in `connection.ts` — it must remain UI-agnostic for potential non-React consumers.
