# Demo GIFs

Scripted, deterministic recordings of the harnx UIs. The LLM responses come
from `harnx-mock-llm` (in `crates/harnx-test-bins`), so every render is
bit-for-bit reproducible — no API keys, no flakiness, no rate limits.

Two pipelines:

| Pipeline      | What it records              | Tooling                        |
|---------------|------------------------------|--------------------------------|
| TUI (`*.tape`) | Terminal UI (`harnx -a …`)   | [VHS](https://github.com/charmbracelet/vhs) |
| Web (`*.mjs`)  | `harnx --serve` playground / arena | [Playwright](https://playwright.dev) + ffmpeg |

## Layout

```
demos/
├── agent.tape                  TUI tape — agent demo
├── tool-confirm.tape           TUI tape — tool confirmation demo
├── render.sh                   TUI orchestrator: mock-llm → vhs → cleanup
├── render-web.sh               Web orchestrator: mock-llm → harnx --serve → playwright → ffmpeg
├── config/                     Self-contained HARNX_CONFIG_DIR (shared by both)
│   ├── config.yaml
│   ├── agents/demo-agent.md
│   ├── agents/tool-confirm-agent.md
│   ├── ask-confirm-hook.sh     Hook script returning "ask" for all tools
│   ├── mcp_servers/time.yaml   Time MCP server (used by tool-confirm demo)
│   └── clients/mock.yaml       openai-compatible client pointed at :3829
├── scripts/                    Mock-LLM response scripts (one entry per request)
│   ├── agent-flow.yaml
│   ├── tool-confirm-flow.yaml
│   ├── playground-flow.yaml
│   └── arena-flow.yaml
├── web/                        Playwright scripts
│   ├── package.json
│   ├── lib.mjs                 Shared helpers (browser, recording, selectors)
│   ├── playground.mjs
│   └── arena.mjs
└── out/                        Rendered artifacts (gitignored)
```

## Render

### TUI

Prerequisite: VHS (`brew install vhs` / `go install github.com/charmbracelet/vhs@latest`).
On Linux you'll also need `ttyd` and `ffmpeg`.

```sh
./demos/render.sh agent
# → demos/out/agent.gif
```

### Web (playground / arena)

Prerequisite: `node` and `ffmpeg`. Playwright + chromium are installed
automatically on first run (into `demos/web/node_modules/`).

```sh
./demos/render-web.sh playground
./demos/render-web.sh arena
# → demos/out/playground.gif and demos/out/arena.gif
```

`render-web.sh` builds release binaries if missing, starts `harnx-mock-llm` on
:3829, starts `harnx --serve` on :8000, runs the matching Playwright script
(which records a WebM via Chromium), then converts to GIF with ffmpeg using a
palette filter for size + colour quality. Everything is torn down on exit.

## Add a new demo

1. Add scripted LLM responses to `demos/scripts/<name>-flow.yaml`. Each entry
   under `turns:` is consumed by one chat-completion request. Schema lives in
   `crates/harnx-test-bins/src/bin/harnx-mock-llm/main.rs` (text chunks, tool
   calls, chunk delay, fallback text). The arena consumes one turn per panel.
2. Add either `demos/<name>.tape` (VHS) or `demos/web/<name>.mjs` (Playwright).
3. Render: `./demos/render.sh <name>` or `./demos/render-web.sh <name>`.

## Replacing the README GIFs

| README image            | Pipeline      | Script                           |
|-------------------------|---------------|----------------------------------|
| `harnx-agent`           | TUI           | `demos/agent.tape`               |
| `harnx-tool-confirm`    | TUI           | `demos/tool-confirm.tape`        |
| `harnx-llm-playground`  | Web           | `demos/web/playground.mjs`       |
| `harnx-llm-arena`       | Web           | `demos/web/arena.mjs`            |
| `harnx-themes`          | TUI (no demo yet) | swap in custom `.tmTheme` files; see `docs/custom-theme.md` |

Once a recording looks right, upload the GIF (GitHub releases, an `assets/`
branch, or `user-attachments` drag-and-drop) and update the matching
`![…](…)` link in the top-level `README.md`.
