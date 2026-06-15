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
- The following `XDG_*` Base Directory variables: `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_BIN_HOME`, `XDG_DATA_DIRS`, `XDG_CONFIG_DIRS`.

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

You can create a `.env.bash` file in your Harnx data directory (typically `~/.local/share/harnx/.env.bash`). This path is overridable via the `HARNX_BASH_ENV_FILE` environment variable. This file uses a plain `KEY=VALUE` format.

- `#` comments and blank lines are ignored.
- The first `=` separates the key from the value (e.g., `KEY=a=b` produces value `a=b`).
- No shell substitution is performed.

**Example `~/.local/share/harnx/.env.bash`:**

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
  - Tool installation directories under `$HOME`: `~/.local/bin`, `~/.local/lib`, `~/.bun`, `~/.asdf`, `~/go/bin`, `~/.cargo`, `~/.nvm`, `~/.cargo/bin`, `~/.mono`, `~/.pyenv`, `~/.rye`, `~/.local/share/claude`, `~/.local/share/opencode`, `~/.local/share/pipx`.
- **Readable**:
  - System C/C++ header directories needed by `cc`, `bindgen`, and crates with native build scripts (Linux: `/usr/include`, `/usr/include/x86_64-linux-gnu`).
  - Common config files under `$HOME`: `~/.gitconfig`, `~/.gitignore`, `~/.gitignore_global`, `~/.tool-versions`.
- **Read+Write**:
  - Cache and module directories under `$HOME`: `~/.cache`, `~/go/pkg`, `~/.npm`, `~/.yarn`, `~/.cargo/registry`, `~/.cargo/git`, `~/.bun/install/cache`, `~/.local/share/pnpm`, `~/.local/share/uv`.

> **Security note:** Tool-install and self-update operations (such as `cargo install`, `nvm install`, `pyenv install`, `rye sync`, `pipx install`, `claude update`, or `opencode self-update`) require explicit write access because these directories are no longer writable by default. You can grant temporary write access using the `--extra-rwx` flag (or the `HARNX_BASH_EXTRA_RWX` environment variable for the bash MCP server), or perform these operations outside the sandbox.

These `$HOME`-relative defaults exist regardless of whether the directory is present on the host (sandbox-run silently skips non-existent paths).

Toolchain-locating environment variables are honoured automatically when set:

| Variable | Effect on sandbox |
|----------|-------------------|
| `CARGO_HOME` | `$CARGO_HOME` added as readable; `$CARGO_HOME/bin` added as executable; `$CARGO_HOME/registry` and `$CARGO_HOME/git` added as read+write. |
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
- All path flags and environment variables support **project-root pseudo-variables** (e.g., `$GIT_ROOT`, `$GIT_COMMON_DIR`, `$NODE_PROJECT_ROOT`, `$CARGO_ROOT`, `$GO_ROOT`). These are resolved at startup against the current working directory and are silently dropped if the current directory is not in a matching project, or if they would expose your home directory. See the [harnx-sandbox-run documentation](sandbox-run.md#project-root-pseudo-variables) for the full list of variables and their semantics.

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

Alternatively, you can persist these in `~/.local/share/harnx/.env.bash`.

#### Non-interactive Editor

Override the `EDITOR` variable to ensure that AI tools that shell out use a non-interactive editor:

```yaml
args: ["-e", "EDITOR=true"]
```

### Sandbox Configuration


The bash MCP sandbox grants project roots read+write+exec access by default. Commands are NOT narrowed by per-call paths; the `bash_exec` and `bash_spawn` tools do not have `inputs` or `outputs` parameters.
Allow tools to use home-directory caches or persistent configuration:

> **Note:** `~/.cargo/bin` is already included in the default allowlist with read+execute permissions, while `~/.cargo/registry`, `~/.cargo/git`, and `~/.npm` are included with read+write permissions. The examples below are only needed if you override the defaults, need additional paths, or require write access for tool installation.

#### Allow pip to cache
```yaml
args: ["--extra-write", "~/.cache/pip"]
```

#### Allow full cargo directory (non-default example)
If you need broader access than the defaults (e.g., for `cargo install` or updates):
```yaml
args: ["--extra-rwx", "~/.cargo"]
```

#### Allow full npm directory (non-default example)
If you need broader access than the default (e.g., for global installs or updates):
```yaml
args: ["--extra-rwx", "~/.npm"]
```

#### Allow cargo registry proc-macros to be loaded (dlopen) by rustc
```yaml
args: ["--extra-rwx", "~/.cargo"]
```


#### Run Chrome or Puppeteer

Chrome uses `/dev/shm` for inter-process shared memory. Starting with this release, `/dev/shm` is already granted write access inside the sandbox, so Puppeteer scripts should start Chrome without any extra `--extra-write` flag.

However, Chrome's own sub-process sandbox tries to create a nested Linux user namespace, which is blocked when it is already running inside birdcage's user namespace. You must launch Chrome with `--no-sandbox` and `--disable-dev-shm-usage` to work around this limitation:

```js
// puppeteer example
const browser = await puppeteer.launch({
  args: ['--no-sandbox', '--disable-dev-shm-usage'],
});
```

These flags are also required in Docker containers and CI environments where the kernel restricts nested user namespaces (`ptrace_scope=1`). They are not related to harnx — they are standard container-mode Chrome flags.
