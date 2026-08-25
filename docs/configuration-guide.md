# Configuration Guide

Harnx uses a modular configuration structure. Global settings are defined in a main `config.yaml`, while LLM providers and tool servers are defined in separate YAML files within dedicated subdirectories.

## Configuration Directory

The configuration files are located in `<user-config-dir>/harnx/`. The exact location depends on your operating system:

| OS      | Path                                                    |
| ------- | ------------------------------------------------------- |
| Windows | `C:\Users\Alice\AppData\Roaming\harnx\`               |
| macOS   | `/Users/Alice/Library/Application Support/harnx/`     |
| Linux   | `/home/alice/.config/harnx/`                           |

To find the config directory on your system:

```sh
harnx --info | grep config_file
```

## Folder Structure

Harnx organizes configuration into the following structure:

```text
~/.config/harnx/
├── config.yaml          # Global settings
├── clients/             # LLM provider configurations
│   ├── openai.yaml
│   └── claude.yaml
├── tool_servers/        # Tool servers (native toolsets and bridged MCP servers)
│   ├── fs.yaml
│   ├── bash.yaml
│   └── exa.yaml
└── agents/              # Agent definitions (.md files)
    └── coder.md
```

## Main Configuration (`config.yaml`)

The `config.yaml` file contains global behavior and appearance settings.

### LLM

- **model**: Specify the default model to use (e.g., `openai:gpt-4o`).

### Behavior

- **stream**: Whether to use streaming for responses. (`true`/`false`)
- **keybindings**: Choose between `emacs` or `vi` style.
- **editor**: Command used to edit input buffers.
- **wrap**: Text wrapping behavior (`no`, `auto`, or a number).
- **wrap_code**: Whether to wrap code blocks. (`true`/`false`)

### Tool Use

- **tool_use**: Set to `false` to disable all tool use globally.
- **use_tools**: Which tools to enable by default (`*` for all).
- **toolsets**: Group tools into named sets for easy assignment.

## Clients (`clients/`)

Each LLM provider is configured in its own YAML file within the `clients/` directory (e.g., `clients/openai.yaml`).

For a complete list of supported providers and their specific configuration options, see the [**LLM Providers Reference**](providers.md).

**Note:** The **filename** (without `.yaml`) is used as the client's ID in `model` settings (e.g., `openai:gpt-4`). Any `name` field inside the file is ignored.

### General Client Options

```yaml
type: openai              # Provider type (openai, claude, gemini, etc.)
api_key: sk-...           # Optional if <NAME>_API_KEY env var is set
api_base: https://...     # Optional custom endpoint
extra:
  connect_timeout: 10     # seconds to establish TCP/TLS connection (default 10)
  read_timeout: 120       # seconds of read inactivity before stalled response fails (default 120)
patches:                  # Patch API requests using jq expressions
  chat_completions:
    - '.body.cache_control = {"type":"ephemeral"}'
```

`extra.connect_timeout` sets how long Harnx waits to establish TCP/TLS connection before request fails. `extra.read_timeout` is per-read inactivity timeout, not cap on total stream duration, so long-but-progressing streaming responses are allowed to continue. Use it so stalled LLM provider surfaces clear error instead of hanging.

### Per-Model Patches

The `patches.chat_completions`, `patches.embeddings`, and `patches.rerank` fields are arrays of **jq filter strings**. Each filter receives the full request object as JSON (`{url, headers, body}`) and must return the modified version.

Filters are applied in sequence. If an expression fails, the request fails with the jq error and the name of the patch source — a skipped patch would send the very request body the patch exists to correct.

Harnx evaluates filters with [jaq](https://github.com/01mf02/jaq), which does not create missing intermediate objects the way jq does. Assign the whole container rather than a nested path:

```yaml
patches:
  chat_completions:
    - '.body.reasoning = {"effort":"high"}'      # works
    # - '.body.reasoning.effort = "high"'        # fails: `.body.reasoning` is null
```

To target specific models, use `if/then` within the expression:

```yaml
type: openai
patches:
  chat_completions:
    - 'if .body.model == "o4-mini" then .body.reasoning_effort = "low" end'
    - 'if .body.model == "o3" then .body.reasoning_effort = "medium" end'
    - 'if .body.model == "gpt-4.1" then .body.reasoning_effort = "high" end'
```

For prefix or pattern matching, use `test` within the expression:

```yaml
patches:
  chat_completions:
    - 'if (.body.model | test("gpt-5.*")) then .body.reasoning_effort = "high" end'
```

## Tool Servers (`tool_servers/`)

Tool servers provide external tools to Harnx. Native toolset servers (such as `harnx-fs-tools`, `harnx-bash-tools`, `harnx-plans-tools`, and `harnx-grep-tools`) run directly without a bridge wrapper, while external stdio MCP servers run via `harnx-mcp-bridge`. Each server is defined in a file under `tool_servers/` (such as `tool_servers/fs.yaml` or `tool_servers/exa.yaml`).

The **filename** (without `.yaml`) is used as the server name.

### Native Tool Server Example

```yaml
command: harnx-fs-tools    # Executable command
args:                      # Filesystem access is opt-in
  - --allow-repo-work
  - --allow-dev-tools
  - --allow-read
  - /srv/reference-data
env:                       # Environment variables
  API_KEY: "..."
description: "Filesystem access tools"
```

Filesystem and bash tool servers deny filesystem access when no allow inputs are configured. Use `--allow-read`, `--allow-write`, `--allow-exec`, or `--allow-rwx` for explicit paths, or opt into `--allow-common-default`, `--allow-dev-tools`, `--allow-repo-work`, or `--allow-all`. Filesystem tools enforce read and write access separately. See [Allowlist migration](migration-allowlist.md) for removed settings.

### External MCP Server Example (`harnx-mcp-bridge`)

To configure an external stdio MCP server, define a server in `tool_servers/` that launches `harnx-mcp-bridge`:

```yaml
command: harnx-mcp-bridge
description: "Web search via Exa API"
args:
  - --name
  - exa
  - --
  - npx
  - "-y"
  - exa-mcp-server
env:
  HARNX_BASH_ENV_PASSTHROUGH: EXA_API_KEY
```

> **Passing secrets to MCP servers.** Two things commonly trip people up:
>
> - **`env:` values are literal — they are *not* shell-expanded.** `$VAR`/`${VAR}` in an `env:` value is passed through verbatim rather than substituted. Writing `API_KEY: "$API_KEY"` sends the literal string `$API_KEY` to the server.
> - **A sandbox wrapper strips the child environment.** If you've wrapped `npx`/`node` with a [harnx sandbox](sandbox-run.md), the server starts with a scrubbed environment, so neither an `env:` value nor an inherited host variable reaches it by default. Forward the specific variable with `HARNX_BASH_ENV_PASSTHROUGH` instead — the sandbox reads it and passes the host value through:
>
>   ```yaml
>   # Server needs EXA_API_KEY, and npx is sandbox-wrapped:
>   env:
>     HARNX_BASH_ENV_PASSTHROUGH: EXA_API_KEY   # forwards the host's EXA_API_KEY
>   ```
>
>   Set the actual secret (`EXA_API_KEY=…`) in `~/.local/share/harnx/.env`.

## NATS Servers (`nats_servers/`)

Harnx supports high-availability distributed mode via NATS. Each cluster is defined in a file like `nats_servers/local.yaml`.

The **filename** (without `.yaml`) is used as the cluster key (e.g., `agent@local`).

```yaml
url: "nats://localhost:4222" # NATS server URL
token: "${NATS_TOKEN}"       # Optional auth token
tls: true                    # Enable TLS
tls_cert: "/path/to/cert"    # Optional client cert
tls_key: "/path/to/key"      # Optional client key
# tls_ca: "/path/to/ca"      # Optional CA (Note: not supported with client cert)
```

See the [NATS HA Guide](nats-ha.md) for more details.

## Example Configuration

A comprehensive reference for the new folder structure and common provider/server examples can be found in the repository at:

[**example_config/**](https://github.com/dobesv/harnx/tree/main/example_config)

This directory includes:
- `config.yaml` with recommended global settings.
- `clients/` examples for OpenAI, Claude, Gemini, Bedrock, Azure, Vertex AI, and more.
- `tool_servers/` examples for filesystem, shell, and web search.
- `agents/` examples for interactive assistants and sub-agents (auto-registered as NATS delegation toolsets).

---

## Other Settings

### RAG

See the [RAG Guide](rag-guide.md) for detailed setup instructions.

### Appearance

- **highlight**: Whether to enable syntax highlighting.
- **light_theme**: Whether to use the light theme.

### Session Titles

Harnx can automatically generate a short, human-readable title for each session
using an LLM. For NATS-backed sessions, titles and regeneration bookkeeping are
stored in canonical session metadata rather than in the conversation transcript.
Titles are shown in session listings, and the active title is also used to set
the terminal window title in the TUI and the browser tab title in the web UI (as
`harnx — <title>`).

- **title_agent**: Name of the agent used to generate titles. When unset, no
  titles are generated. Can be set globally in `config.yaml` or per-agent in an
  agent's front matter (the agent-level value takes precedence). Point it at a
  small, fast chat model.
- **title_update_threshold**: Number of tokens of growth after which the title
  is regenerated. Defaults to `50000`. The first title is generated on the first
  exchange (growth from 0 crosses any non-zero threshold). Set to `0` to disable
  automatic title generation entirely.

Example `config.yaml`:

```yaml
title_agent: title-writer      # an agent configured with a small, fast model
title_update_threshold: 50000
```

You can also override the title agent for a specific agent in its front matter:

```yaml
---
model: openai:gpt-4o
title_agent: title-writer
---
```

**Do not configure a reasoning model** (e.g. OpenAI `o1`/`o3`, DeepSeek-R1) as
the `title_agent`. Such models spend their token budget on internal reasoning
and often return an empty or truncated title. Use a standard chat model.

To set a title manually, use the [`.set title`](tui-guide.md) command in the
TUI. A manually set title freezes automatic regeneration for the rest of the
session.
