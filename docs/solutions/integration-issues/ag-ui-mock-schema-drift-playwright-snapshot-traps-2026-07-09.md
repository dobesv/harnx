---
title: "AG-UI mock schema drift and Playwright snapshot environment traps"
date: 2026-07-09
category: "integration-issues"
problem_type: integration_issue
component: "web, harnx-serve"
root_cause: "MSW mock diverged from AG-UI Zod schema contract; Playwright stale-cache false-positives"
resolution_type: code_fix
severity: high
tags:
  - ag-ui
  - msw
  - zod
  - playwright
  - sse
  - schema-validation
  - snapshot-testing
  - vite-cache
plan_ref: "serve-web-test-coverage"
---

# AG-UI mock schema drift and Playwright snapshot environment traps

## Problem

Two independent issues surfaced during test coverage expansion:

1. **MSW mock schema drift**: Hand-maintained MSW mock (`web/src/mocks/handlers.ts`) emitted AG-UI events with wrong field names, causing `@ag-ui/core` Zod validation to throw and abort the SSE stream.

2. **Playwright snapshot regeneration trap**: Stale Vite dev-server + `node_modules/.vite` cache caused `--update-snapshots` to reuse old code, producing byte-identical (but wrong) screenshots.

## Symptoms

### MSW Mock Schema Drift

```text
# Browser console (@ag-ui/client):
ZodError: [
  { "code": "invalid_type", "path": ["delta"], "message": "Required" }
]

# UI:
- Empty assistant message bubble
- Red error banner: "Invalid event"
- Tool-call args rendered as `{}`
- SSE stream terminates after first invalid event
```

### Playwright Stale Snapshot

```text
# Running: pnpm exec playwright test --update-snapshots
# Expected: Screenshots reflect latest source changes
# Actual: Byte-identical to old baseline despite code fixes
# - reuseExistingServer: !CI reuses stale dev server
# - node_modules/.vite caches stale transpiled modules
```

## Investigation Steps

### MSW Mock

1. Observed empty assistant message + Zod error banner in gallery transcript.
2. Read `@ag-ui/core` Zod schemas in `node_modules`:
   - `TextMessageContentEventSchema`: `{ type: 'TEXT_MESSAGE_CONTENT', messageId: string, delta: string }`
   - `ToolCallArgsEventSchema`: `{ type: 'TOOL_CALL_ARGS', toolCallId: string, delta: string }`
   - `ToolCallResultEventSchema`: `{ type: 'TOOL_CALL_RESULT', messageId: string, toolCallId: string, content: string }`
3. Compared MSW mock fields:
   - `TOOL_CALL_ARGS`: mock had `argsText` (should be `delta`)
   - `TOOL_CALL_RESULT`: mock had `result` (should be `content` + `messageId`)
4. Verified Rust server correct: `ag-ui-core` crate uses `delta`/`content` via serde.
5. Headless probe confirmed fix: no console errors, tool args render correctly.

### Playwright Stale Cache

1. Updated UI source code, ran `--update-snapshots`.
2. Screenshots unchanged despite visible changes in manual browser.
3. Found `reuseExistingServer: !CI` in playwright config.
4. Orphaned Vite dev-servers squatting ports (IPv6 `[::1]`), not killable in some environments.
5. Cleared `node_modules/.vite`, used fresh dedicated port (5180).
6. Verified served module via `curl http://host:port/src/...` before trusting screenshots.

## Root Cause

### MSW Mock

`@ag-ui/client` validates every SSE event with `EventSchemas.parse()` (Zod). A single wrong field name fails validation and aborts the stream. The MSW mock is a hand-maintained replica of the AG-UI wire contract. Over time, mock fields drifted from the `@ag-ui/core` Zod schema:

| Event | Mock Field | Schema Field |
|-------|------------|--------------|
| `TEXT_MESSAGE_CONTENT` | `content` | `delta` |
| `TOOL_CALL_ARGS` | `argsText` | `delta` |
| `TOOL_CALL_RESULT` | `result` | `content` + `messageId` |

The real Rust server (using `ag-ui-core` crate) was already correct — only the mock diverged.

### Playwright Stale Cache

`reuseExistingServer: !CI` reuses any dev-server on the configured port. Combined with Vite's `node_modules/.vite` cache, this can serve stale transpiled code. Result: `--update-snapshots` captures the OLD UI, not the current source.

Additional hazard: orphaned dev-servers squat ports invisibly (IPv6 listeners).

## Solution

### MSW Mock Field Alignment

```typescript
// web/src/mocks/handlers.ts

// WRONG (pre-fix):
{
  type: 'TOOL_CALL_ARGS',
  toolCallId: 'tool-1',
  argsText: '{"query": "example"}'  // ❌ not in schema
}
{
  type: 'TOOL_CALL_RESULT',
  toolCallId: 'tool-1',
  result: '{"data": 123}'  // ❌ wrong field
}

// CORRECT (post-fix):
{
  type: 'TOOL_CALL_ARGS',
  toolCallId: 'tool-1',
  delta: '{"query": "example"}'  // ✅ matches Zod schema
}
{
  type: 'TOOL_CALL_RESULT',
  messageId: 'msg-tool-1',       // ✅ required
  toolCallId: 'tool-1',
  content: '{"data": 123}',     // ✅ correct field
  role: 'tool'                   // optional but recommended
}
```

**Key insight**: When client shows Zod path `["delta"]` required, read the actual Zod schema in `node_modules/@ag-ui/core/dist/index.mjs`. Do not guess field names.

### Playwright Reliable Snapshot Regeneration

1. Clear Vite cache: `rm -rf web/node_modules/.vite`
2. Use guaranteed-free port: `webServer.port: 5180` (check with `netstat` or `ss`)
3. Verify served code before updating snapshots:
   ```bash
   curl http://localhost:5180/src/components/Chat.tsx
   # Confirm latest source is served
   ```
4. Run headless probe to check for console errors:
   ```typescript
   // test
   await page.goto('/');
   expect(page.errors).toEqual([]);  // no Zod or import errors
   ```
5. Only then: `pnpm exec playwright test --update-snapshots`

**Environment note**: Prefer ephemeral port assignment or explicit kill/cleanup between runs. Avoid `reuseExistingServer: true` in CI.

## Why This Works

### MSW Mock

`@ag-ui/client` uses Zod's `EventSchemas.parse()` strictly. Zod validates field names at runtime — `delta` vs `argsText` is a functional difference. When validation fails, the client throws and stops processing the stream. Fixing mock field names to match the schema restores end-to-end event flow.

### Playwright

Vite's cache at `node_modules/.vite` stores pre-bundled dependencies and transpiled code. Stale cache = stale served code. `reuseExistingServer` compounds this by reusing a dev-server started before source changes. Clearing cache + fresh port guarantees the dev-server serves current source.

## Prevention Strategies

### MSW Mock Drift

**Best Practices:**
- Treat MSW mock as a contract test: validate mock events against `@ag-ui/core` Zod schemas
- Subscribe mock handlers to the same `@ag-ui/core` package version as the client (lockstep)
- Add unit test: parse mock events through `EventSchemas.parse()`

```typescript
// test/mocks/schema-conform.test.ts
import { EventSchemas } from '@ag-ui/core';
import { galleryHandlers } from '../src/mocks/handlers';

test('MSW mock events conform to AG-UI schemas', async () => {
  for (const event of mockEventStream) {
    expect(() => EventSchemas.parse(event)).not.toThrow();
  }
});
```

**Code Review Checklist:**
- [ ] Do mock event fields match `@ag-ui/core` Zod schema exactly?
- [ ] When `@ag-ui/core` updates, did MSW mock get updated?
- [ ] Is there a test validating mock events against Zod schemas?

### Playwright Snapshot Reliability

**Best Practices:**
- Always clear Vite cache before `--update-snapshots`: `rm -rf node_modules/.vite`
- Use fresh port per run: `--port=$(pick-free-port)` or dedicated CI port
- Verify served module before trusting snapshots: `curl` or headless probe
- Disable `reuseExistingServer` in CI, or explicitly kill orphaned processes

**Test Setup:**
```typescript
// playwright.config.ts
export default defineConfig({
  use: {
    ...baseConfig,
  },
  webServer: {
    command: 'pnpm dev',
    port: 5180,  // dedicated, check before run
    reuseExistingServer: false,  // always fresh
    timeout: 30000,
  },
});
```

**Code Review Checklist:**
- [ ] Is `reuseExistingServer: false` in CI?
- [ ] Is Vite cache cleared before snapshot regeneration?
- [ ] Did you verify the served code matches source?
- [ ] Are orphaned dev-servers killed before/after run?

## Additional Learnings

### Testing Infrastructure

- **Web had zero unit tests**: Bootstrapped vitest + `@testing-library/react` + `jsdom` (config: `web/vitest.config.ts`, setup: `web/src/test/setup.ts`)
- **Rust uses `cargo nextest`**: Never `cargo test` (per AGENTS.md mandate)

### Rust Bugs Found via Edge-Case Tests

1. **Accept header over-matching**: `contains("text/event-stream")` matched `text/event-streamish` and ignored `q=0`. Fixed: strict media-type token parsing with `q=0` reject (`accept_header_allows_event_stream()` in `lib.rs`).

2. **Empty prompt SSE/RPC inconsistency**: SSE path started a run on empty last-user-message; RPC rejected it. Fixed: SSE now ignores empty/whitespace (join-only), matching RPC.

### Web Attachment Preservation

`toAgUiMessages` must handle `content` as array / string / null / undefined. Null/undefined fallthrough dropped attachments originally. Fixed: fallthrough now preserves attachment parts.

`attachmentToMessageParts` must map ALL content parts (multipart), not just `content[0]`.

## Related Issues

- **Related Solution**: [ag-ui-client-conformance-stock-adapter-2026-07-07.md](./ag-ui-client-conformance-stock-adapter-2026-07-07.md) — `verifyEvents` ordering + runId correlation
- **Related Solution**: [ag-ui-server-protocol-integration-2026-07-04.md](./ag-ui-server-protocol-integration-2026-07-04.md) — AG-UI server integration
- **Related Solution**: [session-rpc-router-handler-contract-2026-07-08.md](./session-rpc-router-handler-contract-2026-07-08.md) — MSW v2 auto-decodes URL params
