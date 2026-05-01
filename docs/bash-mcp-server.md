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

## Filesystem Sandboxing

On Linux and macOS, `harnx-mcp-bash` uses [birdcage](https://github.com/phylum-dev/birdcage) to sandbox child processes. This restricts the agent's ability to read or write files outside of explicitly permitted locations.

### Default Permissions

- **Read/Write/Execute**:
  - The repository root(s) specified via `--root`. This allows agents to run compilers (like `cargo build`) and load native extensions built within the project.
- **Writable**:
  - The system temporary directory (`/tmp` on Linux, `/private/tmp` on macOS).
  - The path in the `$TMPDIR` environment variable, if set.
- **Readable/Executable**:
  - Standard system directories required for bash and common utilities (e.g., `/usr/bin`, `/bin`, `/lib`).
  - Tool installation directories under `$HOME`: `~/.local/bin`, `~/.local/lib`, `~/.bun`, `~/.asdf`, `~/go/bin`.
- **Readable**:
  - System C/C++ header directories needed by `cc`, `bindgen`, and crates with native build scripts (Linux: `/usr/include`, `/usr/include/x86_64-linux-gnu`).
  - Common config files under `$HOME`: `~/.gitconfig`, `~/.gitignore`, `~/.gitignore_global`, `~/.tool-versions`, `~/.local`.
- **Read+Write**:
  - Cache and module directories under `$HOME`: `~/.cache`, `~/go/pkg`.
- **Read+Write+Execute**:
  - Package-manager and version-manager directories under `$HOME`: `~/.npm`, `~/.yarn`, `~/.nvm`, `~/.cargo`, `~/.mono`, `~/.bun/install/cache`, `~/.pyenv`, `~/.rye`.

These `$HOME`-relative defaults exist regardless of whether the directory is present on the host (sandbox-run silently skips non-existent paths).

Toolchain-locating environment variables are honoured automatically when set:

| Variable | Effect on sandbox |
|----------|-------------------|
| `CARGO_HOME` | `$CARGO_HOME/bin` added as executable. |
| `GOROOT` | `$GOROOT` added as executable (Go install). |
| `GOPATH` | `$GOPATH/bin` added as executable; `$GOPATH/pkg` added as read+write. |
| `GOBIN` | `$GOBIN` added as executable. |

### Configuration Options

You can grant additional filesystem access using CLI flags or environment variables. All path flags support the `~` prefix, which is expanded to the user's home directory.

| CLI Flag | Environment Variable | Description |
|----------|----------------------|-------------|
| `--root <path>` | (N/A) | Adds a project root (read/write/exec). |
| `--extra-read <path>` | `HARNX_BASH_EXTRA_READABLE` | Adds a path as read-only. |
| `--extra-write <path>` | `HARNX_BASH_EXTRA_WRITABLE` | Adds a path as writable. |
| `--extra-exec <path>` | `HARNX_BASH_EXTRA_EXEC` | Adds a path to the execution allowlist. |
| `--extra-rwx <path>` | `HARNX_BASH_EXTRA_RWX` | Adds a path with read, write, and execute permissions. |

**Notes:**
- CLI flags can be repeated to add multiple paths.
- Environment variables accept a colon-separated list of paths (e.g., `HARNX_BASH_EXTRA_RWX=/path/one:/path/two`). This applies to all `HARNX_BASH_EXTRA_*` variables.

### Disabling Sandboxing

Use the `--no-sandbox` flag to disable filesystem restrictions entirely.

```yaml
# mcp_servers/bash.yaml
args:
  - --no-sandbox
```

## Common Recipes

### Environment Variables

#### Enable `git push` over SSH

Pass `SSH_AUTH_SOCK` (and optionally `SSH_AGENT_PID`) so the agent's bash process can use your existing SSH agent connection:

```yaml
args: ["-e", "SSH_AUTH_SOCK"]
```

#### Enable GitHub CLI (`gh`)

Pass `GH_TOKEN` or `GITHUB_TOKEN`:

```yaml
args: ["-e", "GITHUB_TOKEN"]
```

Alternatively, you can persist these in `~/.config/harnx/.env.bash`.

#### Non-interactive Editor

Override the `EDITOR` variable to ensure that AI tools that shell out use a non-interactive editor:

```yaml
args: ["-e", "EDITOR=true"]
```

### Sandbox Configuration

Allow tools to use home-directory caches or persistent configuration:

#### Allow cargo to cache
```yaml
args: ["--extra-write", "~/.cargo"]
```

#### Allow pip to cache
```yaml
args: ["--extra-write", "~/.cache/pip"]
```

#### Allow npm globals
```yaml
args: ["--extra-write", "~/.npm"]
```

#### Read-only cargo registry
```yaml
args: ["--extra-read", "~/.cargo/registry"]
```

#### Allow cargo registry proc-macros to be loaded (dlopen) by rustc
```yaml
args: ["--extra-rwx", "~/.cargo"]
```
