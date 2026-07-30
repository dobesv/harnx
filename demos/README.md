# Demo GIFs

Scripted, deterministic recordings of the harnx TUI. The LLM responses come
from `harnx-mock-llm`, and faked tool results come from `harnx-mock-mcp` (both
in `crates/harnx-test-bins`), so every render is reproducible — no API keys, no
flakiness, no rate limits, and nothing actually runs against your machine. The
agent demo, for example, shows a full read → edit → test coding loop where the
mock LLM emits the tool calls and the mock MCP server returns canned results;
harnx renders its real tool-call/tool-result UI on top.

## Layout

```
demos/
├── agent.tape                  TUI tape — agent demo
├── tool-confirm.tape           TUI tape — tool confirmation demo
├── themes-dark.tape            TUI tape — themes dark demo
├── themes-light.tape           TUI tape — themes light demo
├── render.sh                   Orchestrator: runs VHS + harnx-mock-llm
├── config/                     Self-contained HARNX_CONFIG_DIR for all demos
│   ├── config.yaml
│   ├── dark.tmTheme            Theme used by the themes demo
│   ├── light.tmTheme
│   ├── ask-confirm-hook.sh     Hook returning "ask" for all tools
│   ├── agents/                 Demo agent definitions (demo/code/tool-confirm)
│   ├── clients/                openai-compatible client pointed at :3829
│   │   └── mock.yaml
│   └── tool_servers/
│       ├── dev.yaml            Wires harnx-mock-mcp for the agent demo
│       └── time.yaml           Time MCP server (tool-confirm demo)
└── scripts/                    Mock-LLM turns (*-flow) + mock-MCP results (*-tools)
    ├── agent-flow.yaml         Scripted LLM responses for agent demo
    ├── agent-tools.yaml        Canned tool results served by harnx-mock-mcp
    ├── tool-confirm-flow.yaml
    ├── themes-dark-flow.yaml
    └── themes-light-flow.yaml
```

## Render

Prerequisite: VHS (`brew install vhs` / `go install github.com/charmbracelet/vhs@latest`).
On Linux you'll also need `ttyd` and `ffmpeg`.

```sh
./demos/render.sh agent
# → demos/out/agent.gif

# For themes:
./demos/render.sh themes-dark && ./demos/render.sh themes-light
# → demos/out/themes-dark.gif and demos/out/themes-light.gif
```

## Add a new demo

1. Add scripted LLM responses to `demos/scripts/<name>-flow.yaml`. Each entry
   under `turns:` is consumed by one chat-completion request. Schema lives in
   `crates/harnx-test-bins/src/bin/harnx-mock-llm/main.rs` (text chunks, tool
   calls, chunk delay, fallback text).
2. To show tool use without running anything real, have the mock LLM emit tool
   calls and serve faked results from `harnx-mock-mcp`: add a
   `demos/scripts/<name>-tools.yaml` (tools + ordered `responses`, see
   `agent-tools.yaml`), wire it via a `demos/config/tool_servers/<server>.yaml`
   (`command: harnx-mock-mcp`, `args: [--script, demos/scripts/<name>-tools.yaml]`),
   and list the `<server>_<tool>` names in the agent's `use_tools`. Schema lives
   in `crates/harnx-test-bins/src/bin/harnx-mock-mcp/server.rs`.
3. Add `demos/<name>.tape` (VHS).
4. Render: `./demos/render.sh <name>`.

## Replacing the README GIFs

| README image            | Script                           |
|-------------------------|----------------------------------|
| `harnx-agent`           | `demos/agent.tape`               |
| `harnx-tool-confirm`     | `demos/tool-confirm.tape`        |
| `harnx-themes`           | `demos/themes-dark.tape` and `demos/themes-light.tape` |

Once a recording looks right, upload the GIF (GitHub releases, an `assets/`
branch, or `user-attachments` drag-and-drop) and update the matching
`![…](…)` link in the top-level `README.md`.
