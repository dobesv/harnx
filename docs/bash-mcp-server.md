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

## Override the toolset name

Pass `--name <NAME>` when two bash servers need to run in the same scope. The
name controls NATS registration and the agent-visible tool prefix. For example:

```yaml
command: harnx-bash-tools
args: [--name, review, --allow-repo-work]
```

This server registers separately from the default `bash` server, and its tools
appear as `review_exec`, `review_spawn`, and so on. Names must start with an
ASCII letter or digit; remaining characters may also be hyphens or underscores.

## Restrict published tools

By default, `harnx-bash-tools` publishes all built-in tools (`exec`, `spawn`, `wait`, `terminate`, `read_exec_log`) and any loaded command templates.

Pass `--enable-tool <glob>` to limit published and callable tools to those matching the pattern. Tools that do not match are not registered and cannot be invoked. Repeat `--enable-tool` to specify multiple patterns. Without `--enable-tool`, all tools are published.

Matching evaluates against raw tool names (such as `exec`), not agent-prefixed names (`bash_exec`). Invalid glob patterns or empty values cause startup failure.

In cloud deployments, this allows exposing predefined command templates while removing arbitrary command execution:

```yaml
command: harnx-bash-tools
args:
  - --allow-repo-work
  - --tools-dir
  - /etc/harnx/tools
  - --enable-tool
  - deploy_*
  - --enable-tool
  - status
```

In this configuration, only template tools matching `deploy_*` and the `status` tool are exposed; the generic `exec` tool is omitted.

## Built-in tools and execution model

`harnx-bash-tools` exposes five built-in tools for running and managing commands:

| Tool | Purpose | Key parameters |
| :--- | :--- | :--- |
| `exec` | Synchronous foreground execution | `command`, `timeout_secs`, output filters (`head_lines`, `tail_lines`, `max_output_bytes`) |
| `spawn` | Background execution with optional wait | `command`, optional nested `wait` configuration (`timeout_secs`, `head_lines`, `tail_lines`, `max_output_bytes`, `grep`) |
| `wait` | Poll or wait on a background process | `execution_id`, flat wait and filter options (`timeout_secs`, `head_lines`, `tail_lines`, `max_output_bytes`, `grep`) |
| `terminate` | Stop a background process | `execution_id`, `signal` (`SIGTERM` default, `SIGKILL`, `SIGINT`, `SIGHUP`) |
| `read_exec_log` | Read server-owned execution logs | `execution_id`, `stream` (`stdout`/`stderr`), paging and filter options |

### Spawning processes with nested wait

The `spawn` tool starts a command in the background. It takes an optional nested `wait` object that controls whether and how long the caller blocks for completion:

```json
{
  "command": "cargo test",
  "wait": {
    "timeout_secs": 30,
    "tail_lines": 50
  }
}
```

- **Omitted `wait`**: Returns immediately with `execution_id` and log file paths without waiting for the process to exit.
- **Empty object (`wait: {}`)**: Uses the default timeout of 120 seconds (two minutes).
- **Immediate poll (`wait: { timeout_secs: 0 }`)**: Checks current status immediately without blocking. Note that `timeout_secs: 0` in `wait` means an instant poll, unlike `exec` where `0` means an unlimited deadline.
- **Output filtering**: The nested `wait` object accepts `head_lines`, `tail_lines`, `max_output_bytes`, and regex `grep` to filter or truncate command output before returning it. It does not accept `execution_id`, which is generated by `spawn`.
- **Process lifecycle on wait expiry**: If the command finishes within the wait duration, `spawn` returns exit status, exit code, and captured output. If the command runs longer than the wait duration, `spawn` returns running status with partial output and the `execution_id`. **Wait expiry never terminates the process.** The process continues running in the background.
- **Cache TTL recommendation**: Bounded wait durations under 5 minutes (such as 30 seconds or the 120-second default) are recommended. Many LLM providers cache prompt context for 5 minutes; returning before the cache expires allows the agent turn to reuse cached context. Explicit longer waits remain supported without artificial caps.
- **Cancellation during the initial wait**: If the caller's turn is cancelled while the initial nested `wait` is still in progress, the call reports an interrupted result without an `execution_id` (the first response never reached the caller), and the command keeps running. Because the server hands the process over to the response path before the first reply, such a cancelled call can leave the running process unreachable through `wait` or `terminate`. Prefer bounded initial waits so the `execution_id` is delivered quickly, then continue with follow-up `wait` calls.

### Follow-up with `wait` and `terminate`

When a command outlasts its initial wait:

1. **Continue waiting**: Call `wait` with `execution_id`. The standalone `wait` tool takes flat arguments (`execution_id`, `timeout_secs`, and output filters). Its default timeout is 120 seconds, and `timeout_secs: 0` performs an immediate status check. Expiry still leaves the process running.
2. **Stop the process**: Call `terminate` with `execution_id`. It defaults to `SIGTERM`, and accepts `SIGKILL`, `SIGINT`, or `SIGHUP`. Process termination is handled exclusively through `terminate`.
3. **Inspect full output**: Call `read_exec_log` with `execution_id` and stream (`stdout` or `stderr`) to page through log files when output was truncated.

### Foreground execution (`exec`)

`exec` runs commands synchronously with a 24-hour default deadline. It is best suited for quick one-off commands (such as git checks, ast-grep queries, and single utility calls) where process lifecycle tracking is unnecessary. For tests, builds, and commands that may run long or stall, prefer `spawn` with nested `wait`.

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
- `--enable-tool <glob>` limits callable tools to matched names, omitting unrestricted tools like `exec`.
- Non-Unix platforms don't have birdcage enforcement.

See [Allowlist migration](migration-allowlist.md) when updating an existing config. Old path CLI options are rejected with exit status 1 so stale shipped or shadowed YAML fails visibly.
