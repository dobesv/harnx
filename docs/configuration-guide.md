# Configuration Guide

Harnx uses a modular configuration structure. Global settings are defined in a main `config.yaml`, while LLM providers, MCP servers, and ACP servers are defined in separate YAML files within dedicated subdirectories.

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
├── mcp_servers/         # MCP server configurations
│   ├── fs.yaml
│   └── bash.yaml
├── acp_servers/         # ACP server overrides (optional)
│   └── custom.yaml
└── agents/              # Agent definitions (.md files)
    └── coder.md
```

## Main Configuration (`config.yaml`)

The `config.yaml` file contains global behavior and appearance settings.

### LLM

- **model**: Specify the default model to use (e.g., `openai:gpt-4o`).

### Behavior

- **stream**: Whether to use streaming for responses. (`true`/`false`)
- **save_session**: Whether to save session history. (`true`/`false`)
- **cleanup_inactive_sessions_days**: Number of days of inactivity (filesystem mtime) after which a session's transcript (`{id}.yaml`) and its attachments directory (`{id}.attachments/`) are permanently deleted. Deletion is best-effort, permanent, and runs at startup and hourly thereafter in all modes. Set to `0` or leave unset to disable (default). (Example: `14`)
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
patches:                  # Patch API requests using jq expressions
  chat_completions:
    - '.body.cache_control = {"type":"ephemeral"}'
```

### Per-Model Patches

The `patches.chat_completions`, `patches.embeddings`, and `patches.rerank` fields are arrays of **jq filter strings**. Each filter receives the full request object as JSON (`{url, headers, body}`) and must return the modified version.

Filters are applied in sequence. If an expression fails, a warning is logged and that specific patch is skipped.

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

## MCP Servers (`mcp_servers/`)

Model Context Protocol (MCP) servers provide external tools. Each server is defined in a file like `mcp_servers/fs.yaml`.

The **filename** (without `.yaml`) is used as the server name.

```yaml
command: harnx-mcp-fs     # Executable command
args: ["--root", "."]     # Optional arguments
env:                      # Environment variables
  API_KEY: "..."
roots:                    # Directories the server can access
  - "$HOME/projects"
description: "Filesystem access tools"
```

## ACP Servers (`acp_servers/`)

Agent Client Protocol (ACP) servers allow Harnx to delegate tasks to other agents.

### Auto-Registration

All agents defined in the `agents/` directory are **automatically registered** as ACP servers. You can call them from any other agent without manual configuration.

### Overrides

If you need to customize an agent's ACP settings (e.g., add environment variables or change timeouts), create a file in `acp_servers/` with the same name as the agent (e.g., `acp_servers/coder.yaml`).

```yaml
command: harnx-acp-server
args: ["coder"]
env:
  DEBUG: "true"
idle_timeout_secs: 600
```

## Example Configuration

A comprehensive reference for the new folder structure and common provider/server examples can be found in the repository at:

[**example_config/**](https://github.com/dobesv/harnx/tree/main/example_config)

This directory includes:
- `config.yaml` with recommended global settings.
- `clients/` examples for OpenAI, Claude, Gemini, Bedrock, Azure, Vertex AI, and more.
- `mcp_servers/` examples for filesystem, shell, and web search.
- `agents/` and `acp_servers/` usage documentation.

---

## Other Settings

### RAG

See the [RAG Guide](rag-guide.md) for detailed setup instructions.

### Appearance

- **highlight**: Whether to enable syntax highlighting.
- **light_theme**: Whether to use the light theme.
