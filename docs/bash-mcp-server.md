# Bash MCP Server

## Overview

`harnx-mcp-bash` is the MCP server that exposes shell command execution to agents. Filesystem sandboxing (via [birdcage](https://github.com/phylum-dev/birdcage)) is enabled by default on Unix-like systems; it is unavailable on Windows.

The server starts child bash processes with a curated environment, NOT the full host environment. This prevents sensitive information (like API keys or other secrets) in the parent shell from being accidentally exposed to the LLM agent's tool calls. Environment curation is independent of sandboxing — it applies on every platform, including Windows where filesystem sandboxing is unavailable.

## Default Environment Allowlist

By default, only a minimal set of host environment variables is passed through to the child bash process:

- `HOME`
- `PATH`
- `LANG`
- `LANGUAGE`
- `USER`
- `SHELL`
- `TERM`
- `DISPLAY`
- `EDITOR`
- `NODE_OPTIONS`
- `NODE_EXTRA_CA_CERTS`
- `PWD`
- `SHLVL`
- `LOGNAME`
- `TMPDIR`
- `TMP`
- `TEMP`
- All variables prefixed with `XDG_*` (e.g., `XDG_CONFIG_HOME`, `XDG_RUNTIME_DIR`)

## Adding Extra Variables

You can add extra environment variables to the child process in three ways. These methods are additive.

### 1. Per-server CLI flags

Use the `-e` or `--env` flags in your MCP server configuration. This is useful for passing specific variables or setting explicit values.

```yaml
# mcp_servers/bash.yaml
command: harnx-mcp-bash
args:
  - -e
  - GITHUB_TOKEN              # Pass through from host env
  - -e
  - GIT_AUTHOR_NAME=My Bot    # Set an explicit value
```

### 2. Environment variable

Use `HARNX_BASH_ENV_PASSTHROUGH` to specify a comma-separated list of host environment variable names to pass through.

```yaml
# mcp_servers/bash.yaml
env:
  HARNX_BASH_ENV_PASSTHROUGH: GITHUB_TOKEN,SSH_AUTH_SOCK
```

### 3. Dotfile (`.env.bash`)

You can create a `.env.bash` file in your Harnx configuration directory (typically `~/.config/harnx/.env.bash`). This file uses a plain `KEY=VALUE` format.

- `#` comments and blank lines are ignored.
- The first `=` separates the key from the value (e.g., `KEY=a=b` produces value `a=b`).
- No shell substitution is performed.

**Example `~/.config/harnx/.env.bash`:**

```text
# GitHub Token
GITHUB_TOKEN=ghp_xxx

# SSH agent (resolved at MCP server startup)
SSH_AUTH_SOCK=/tmp/ssh-XXXX/agent.123
```

## Precedence

When a variable is defined in multiple places, the value from the source highest in this list wins:

1. CLI flags (`-e VAR=VALUE`)
2. `HARNX_BASH_ENV_PASSTHROUGH` (value taken from host environment)
3. `.env.bash` dotfile (value from file)
4. Default allowlist (value from host environment)

## Common Recipes

### Enable `git push` over SSH

Pass `SSH_AUTH_SOCK` (and optionally `SSH_AGENT_PID`) so the agent's bash process can use your existing SSH agent connection:

```yaml
args: ["-e", "SSH_AUTH_SOCK"]
```

### Enable GitHub CLI (`gh`)

Pass `GH_TOKEN` or `GITHUB_TOKEN`:

```yaml
args: ["-e", "GITHUB_TOKEN"]
```

Alternatively, you can persist these in `~/.config/harnx/.env.bash`.

### Non-interactive Editor

Override the `EDITOR` variable to ensure that AI tools that shell out use a non-interactive editor:

```yaml
args: ["-e", "EDITOR=true"]
```
