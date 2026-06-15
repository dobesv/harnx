# Demo GIFs

Scripted, deterministic recordings of the harnx UIs. The LLM responses come
from `harnx-mock-llm`, and faked tool results come from `harnx-mock-mcp` (both
in `crates/harnx-test-bins`), so every render is reproducible — no API keys, no
flakiness, no rate limits, and nothing actually runs against your machine. The
agent demo, for example, shows a full read → edit → test coding loop where the
mock LLM emits the tool calls and the mock MCP server returns canned results;
harnx renders its real tool-call/tool-result UI on top.

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
├── themes-dark.tape            TUI tape — themes dark demo
├── themes-light.tape           TUI tape — themes light demo
├── render.sh                   TUI orchestrator: mock-llm → vhs → cleanup
├── render-web.sh               Web orchestrator: mock-llm → harnx --serve → playwright → ffmpeg
├── assets/                     Committed outputs
├── config/                     Self-contained HARNX_CONFIG_DIR (shared by both)
│   ├── config.yaml
│   ├── agents/demo-agent.md
│   ├── agents/code-agent.md       Coding agent for the agent demo (uses dev_* tools)
│   ├── agents/tool-confirm-agent.md
│   ├── ask-confirm-hook.sh     Hook script returning "ask" for all tools
│   ├── mcp_servers/time.yaml   Time MCP server (used by tool-confirm demo)
│   ├── mcp_servers/dev.yaml    Mock dev tools (harnx-mock-mcp) for the agent demo
│   └── clients/mock.yaml       openai-compatible client pointed at :3829
├── scripts/                    Mock-LLM turns (*-flow) + mock-MCP results (*-tools)
│   ├── agent-flow.yaml         Mock-LLM turns for the agent coding session
│   ├── agent-tools.yaml        Canned tool results served by harnx-mock-mcp
│   ├── tool-confirm-flow.yaml
│   ├── themes-dark-flow.yaml
│   ├── themes-light-flow.yaml
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

# For themes:
./demos/render.sh themes-dark && ./demos/render.sh themes-light
# → demos/out/themes-dark.gif and demos/out/themes-light.gif
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
2. To show tool use without running anything real, have the mock LLM emit tool
   calls and serve faked results from `harnx-mock-mcp`: add a
   `demos/scripts/<name>-tools.yaml` (tools + ordered `responses`, see
   `agent-tools.yaml`), wire it via a `demos/config/mcp_servers/<server>.yaml`
   (`command: harnx-mock-mcp`, `args: [--script, demos/scripts/<name>-tools.yaml]`),
   and list the `<server>_<tool>` names in the agent's `use_tools`. Schema lives
   in `crates/harnx-test-bins/src/bin/harnx-mock-mcp/server.rs`.
3. Add either `demos/<name>.tape` (VHS) or `demos/web/<name>.mjs` (Playwright).
4. Render: `./demos/render.sh <name>` or `./demos/render-web.sh <name>`.

## Replacing the README GIFs

| README image            | Pipeline      | Script                           |
|-------------------------|---------------|----------------------------------|
| `harnx-agent`           | TUI           | `demos/agent.tape`               |
| `harnx-tool-confirm`    | TUI           | `demos/tool-confirm.tape`        |
| `harnx-llm-playground`  | Web           | `demos/web/playground.mjs`       |
| `harnx-llm-arena`       | Web           | `demos/web/arena.mjs`            |
| `harnx-themes`          | TUI           | `demos/themes-dark.tape` and `demos/themes-light.tape` |

Once a recording looks right, upload the GIF (GitHub releases, an `assets/`
branch, or `user-attachments` drag-and-drop) and update the matching
`![…](…)` link in the top-level `README.md`.
