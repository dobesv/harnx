# End-to-End Tests

This directory contains Playwright e2e tests for the web UI.

## Running Tests

```bash
cd web
pnpm test:e2e
```

Playwright automatically starts a dev server on port 5174 with MSW enabled (see below).
No manual server startup required.

## First-Time Setup

Install Playwright's Chromium browser:

```bash
cd web
pnpm exec playwright install chromium
```

On first run in CI or fresh environments, `--with-deps` installs system dependencies too:

```bash
pnpm exec playwright install --with-deps chromium
```

## Updating Screenshots

When UI changes are intentional, update screenshots:

```bash
pnpm test:e2e:update
```

This runs Playwright with `--update-snapshots`. Review the changed PNGs in git diff
before committing. Screenshots are committed for human review, not as a strict pixel-perfect gate.

## MSW (Mock Service Worker)

E2e tests use MSW to mock the backend API:

- **E2e mode**: `VITE_ENABLE_MSW=true` enables MSW. Playwright's config starts the dev server with this flag.
- **Normal dev/build**: MSW is OFF. Running `pnpm dev` or `pnpm build` without the flag uses the real backend.

## Scenario Selection

Tests drive different MSW scenarios via the `?scenario=<name>` URL param:

| Scenario | Description |
|----------|-------------|
| `happy` | Normal success responses (default) |
| `agentsFail` | `/v1/agents` returns 500 |
| `sessionsFail` | `/v1/agents/:agent/sessions` returns 404 |
| `sendFail` | RPC POST returns 500 |

See `web/src/mocks/handlers.ts` for handler implementations.

## Dev Server Port

Playwright runs the dev server on port **5174** (configured in `playwright.config.ts`).
The normal `pnpm dev` uses port 5173; e2e uses 5174 to avoid conflicts.

## Interactive UI Mode

For debugging:

```bash
pnpm test:e2e:ui
```

Opens Playwright's interactive UI with time-travel debugging.
