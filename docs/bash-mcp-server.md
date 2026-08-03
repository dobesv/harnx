# Bash toolset server

`harnx-bash-tools` runs commands and script snippets through a birdcage filesystem sandbox on Linux and macOS. It runs as a native toolset server by default; `--mcp-stdio` keeps stdio MCP compatibility.

## Configure filesystem access

Filesystem access is deny-all when no allow inputs are set. Enable named batches for common use cases, then add explicit paths for anything outside those batches:

```yaml
command: harnx-bash-tools
args:
  - --allow-common-default
  - --allow-dev-tools
  - --allow-repo-work
  - --allow-read
  - ~/.config/my-tool
  - --allow-rwx
  - ~/.cache/my-tool
```

Write and execute grants also grant read access. Every privileged grant applies the `$HOME` guard: `$HOME` and its ancestors can be read, but aren't made writable or executable by allow inputs.

| Option | Access |
| :--- | :--- |
| `--allow-read <PATH>` | Read |
| `--allow-write <PATH>` | Read and write |
| `--allow-exec <PATH>` | Read and execute |
| `--allow-rwx <PATH>` | Read, write, and execute |
| `--allow-common-default` | Common operating-system commands, libraries, pseudo-filesystems, and temporary directories |
| `--allow-dev-tools` | Development toolchains, package caches, and supported tool configuration |
| `--allow-repo-work` | Detected Git, Cargo, Node, and Go project paths; Git common directory; session working directory |
| `--allow-all` | Full filesystem request, subject to `$HOME` guard |

All batches are opt-in. `--allow-repo-work` grants detected project roots and session working directory read/write/execute, while Git common directory receives read/write only.

## Batch contents

`--allow-common-default` grants standard system executable and library paths, broad read/execute access to `/proc`, `/dev`, `/sys`, `/etc`, selected `/run` paths, and read/write/execute access to `/tmp` and `/dev/shm`. macOS receives corresponding system paths and temporary directories.

`--allow-dev-tools` grants supported tool locations under `$HOME` and paths derived from `CARGO_HOME`, `GOROOT`, `GOPATH`, `GOBIN`, `GOMODCACHE`, `GOCACHE`, and `HOMEBREW_PREFIX`. Main home-directory grants are:

| Permission | Paths |
| :--- | :--- |
| Read | `~/.gitconfig`, `~/.gitignore`, `~/.gitignore_global`, `~/.tool-versions` |
| Read + execute | `~/.local/bin`, `~/.local/lib`, `~/.bun`, `~/.asdf`, `~/go/bin`, `~/.cargo`, `~/.nvm`, `~/.mono`, `~/.pyenv`, `~/.rye`, `~/.rustup`, `~/.local/share/{claude,opencode,pipx}` |
| Read + write | `~/.cache`, `~/go/pkg`, `~/.npm`, `~/.yarn`, `~/.cargo/{registry,git}`, `~/.bun/install/cache`, `~/.local/share/{pnpm,uv}` |
| Read + write + execute | `~/.config/go` |

Use explicit `--allow-*` options for app-specific paths. Shipped package configs show examples for Chromium, dcg, Git config, rtk, Ruff cache, and Chrome. They explicitly upgrade `~/.cache` to read/write/execute because some build tools execute cached artifacts.

## Environment variables

Path lists use platform path-list syntax:

- `HARNX_TOOLS_ALLOW_READ`
- `HARNX_TOOLS_ALLOW_WRITE`
- `HARNX_TOOLS_ALLOW_EXEC`
- `HARNX_TOOLS_ALLOW_RWX`

Batch toggles accept `1`, `true`, `yes`, or `on`:

- `HARNX_TOOLS_ALLOW_COMMON_DEFAULT`
- `HARNX_TOOLS_ALLOW_DEV_TOOLS`
- `HARNX_TOOLS_ALLOW_REPO_WORK`
- `HARNX_TOOLS_ALLOW_ALL`

`HARNX_BASH_ENV_PASSTHROUGH` is a comma-separated list of host environment variable names copied into child processes. `--env NAME` inherits one variable; `--env NAME=value` sets a value.

## Native and stdio modes

Native mode is the default and is used by files in `tool_servers/`:

```yaml
command: harnx-bash-tools
args: [--allow-common-default, --allow-dev-tools, --allow-repo-work]
```

Use `--mcp-stdio` only for clients that need an MCP stdio server. Both modes resolve the same immutable allowlist at startup; stdio mode doesn't negotiate filesystem access.

## Security behavior

- No allow inputs means no filesystem grants.
- Network access remains enabled unless sandbox-run is invoked with `--no-network`.
- `--no-sandbox` disables filesystem enforcement. Use it only when another isolation boundary exists.
- Absolute shebang interpreters must already be executable through a batch or explicit allow rule. Interpreter paths don't receive dynamic grants.
- Non-Unix platforms don't have birdcage enforcement.

See [Allowlist migration](migration-allowlist.md) when updating an existing config. Old path CLI options are rejected with exit status 1 so stale shipped or shadowed YAML fails visibly.
