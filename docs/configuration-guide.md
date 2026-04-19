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

**Note:** The filename is for organization only. The client's ID used in `model` settings (e.g., `myclient:gpt-4`) is determined by the `name` field inside the configuration file.

### General Client Options

```yaml
type: openai              # Provider type (openai, claude, gemini, etc.)
name: my-openai           # Client ID for model strings (e.g., my-openai:gpt-4)
api_key: sk-...           # Optional if <NAME>_API_KEY env var is set
api_base: https://...     # Optional custom endpoint
patch:                    # Patch API requests (url, headers, body)
  chat_completions:
    body:
      cache_control:
        type: ephemeral
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
command: harnx
args: ["--acp", "coder"]
env:
  DEBUG: "true"
idle_timeout_secs: 600
```

## Example Configuration

A comprehensive reference for the new folder structure and common provider/server examples can be found in the repository at:

[**example_config/**](https://github.com/dobesv/harnx/tree/main/example_config)

This directory includes:
- `config.yaml` with recommended global settings.
- `clients/` examples for OpenAI, Claude, Gemini, and Ollama.
- `mcp_servers/` examples for filesystem, shell, and web search.
- `agents/` and `acp_servers/` usage documentation.

---

## Other Settings

### Default Session

- **tui_default_session**: Session spec applied when entering TUI mode. (Accepted under the legacy alias `repl_default_session` for backward compatibility.)
- **cmd_default_session**: Session spec applied when entering CMD mode.
- **agent_default_session**: Session identifier used when starting an agent.

### RAG

See the [RAG Guide](rag-guide.md) for detailed setup instructions.

### Appearance

- **highlight**: Whether to enable syntax highlighting.
- **light_theme**: Whether to use the light theme.
