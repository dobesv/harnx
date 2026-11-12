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

By default, `harnx-bash-tools` publishes all built-in tools (`exec`, `spawn`, `status`, `wait`, `terminate`, `read_exec_log`, `rollback_file`) and any loaded command templates.

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
  - service_status
```

In this configuration, only template tools matching `deploy_*` and the `service_status` tool are exposed; the generic `exec` tool is omitted.

Built-in tool names are reserved: a command template named `exec`, `spawn`, `status`, `wait`, `terminate`, `read_exec_log` or `rollback_file` fails startup when you pass it with `--tool` or `--tools-dir`, and is skipped with a warning when it is auto-discovered. Give templates distinct names such as `service_status`.

## Built-in tools and execution model

`harnx-bash-tools` exposes seven built-in tools for running and managing commands:

| Tool | Purpose | Key parameters |
| :--- | :--- | :--- |
| `exec` | Synchronous foreground execution | `command`, `timeout_secs` (1 to 86400), output filters (`head_lines`, `tail_lines`, `max_output_bytes`) |
| `spawn` | Background execution with optional wait | `command`, `timeout_secs` (kill deadline, 1 to 86400), optional nested `wait` configuration (`seconds`, `forever`, `head_lines`, `tail_lines`, `max_output_bytes`, `grep`) |
| `status` | Report a background process's state without waiting | `execution_id`, output filters (`head_lines`, `tail_lines`, `max_output_bytes`, `grep`) |
| `wait` | Wait on a background process | `execution_id`, `seconds` (1 to 240) or `forever`, output filters |
| `terminate` | Stop a background process | `execution_id`, `signal` (`SIGTERM` default, `SIGKILL`, `SIGINT`, `SIGHUP`) |
| `read_exec_log` | Read server-owned execution logs | `execution_id`, `stream` (`stdout`/`stderr`), paging and filter options |
| `rollback_file` | Restore a repository to a prior harnx history snapshot | `commit_id` (the SHA from the `commit <sha>` line of a prior tool response's diff) |

`spawn`, `status` and `wait` reject arguments they do not declare. In particular, `wait` no longer takes `timeout_secs` (use `seconds`), and a nested `wait` object does not either.

### Deadlines versus waits

Two independent limits apply to a background command:

- **`timeout_secs` on `spawn`** is the process's own kill deadline, from 1 to 86400 seconds (24 hours, the default). When it passes, the whole process group is killed. A later `status` or `wait` reports `status: timed_out` with the summary `execution_id '<id>' killed after its <n>s deadline`. This is a normal result, not an error.
- **A wait** only bounds how long a call holds the caller. It never stops the process.

`exec` and command templates use the same `timeout_secs` range and default; `0` is rejected.

### Spawning processes with nested wait

The `spawn` tool starts a command in the background. It takes an optional nested `wait` object that controls whether and how long the caller blocks for completion:

```json
{
  "command": "cargo test",
  "timeout_secs": 1800,
  "wait": {
    "seconds": 30,
    "tail_lines": 50
  }
}
```

- **Omitted `wait`**: Returns immediately with `execution_id` and log file paths without waiting for the process to exit.
- **Empty object (`wait: {}`)**: Waits up to the default of 240 seconds.
- **Fixed wait (`wait: { seconds: N }`)**: Waits up to `N` seconds, from 1 to 240. Other values are rejected; to check without waiting, omit `wait` and call `status`.
- **Unbounded wait (`wait: { forever: true }`)**: Waits until the process exits, which the process's own `timeout_secs` deadline bounds. It cannot be combined with `seconds`.
- **Output filtering**: The nested `wait` object accepts `head_lines`, `tail_lines`, `max_output_bytes`, and regex `grep` to filter or truncate command output before returning it. It does not accept `execution_id`, which is generated by `spawn`.
- **Cancelled initial wait**: Over MCP stdio, MCP HTTP and the k8s-sandbox proxy there is no partial-result store, so if the caller cancels a `spawn` call during its nested wait, the `execution_id` is never returned. The process stays tracked and is killed at its `timeout_secs` deadline. The native NATS route records the `execution_id` as a partial result instead.
- **Process lifecycle on wait expiry**: If the command finishes within the wait duration, `spawn` returns exit status, exit code, and captured output. If the command runs longer, `spawn` returns running status with partial output and the `execution_id`. **Wait expiry never terminates the process.**

### Follow-up with `status`, `wait` and `terminate`

When a command outlasts its initial wait:

1. **Check without waiting**: Call `status` with `execution_id`. It returns immediately with `status: running` and `running_for`, `status: exited` and `exit_code`, or `status: timed_out`, plus output filtered by the same options as `wait`.
2. **Continue waiting**: Call `wait` with `execution_id`. It takes flat arguments (`execution_id`, `seconds` or `forever`, and output filters) with a default of 240 seconds. Expiry still leaves the process running.
3. **Stop the process**: Call `terminate` with `execution_id`. It defaults to `SIGTERM`, and accepts `SIGKILL`, `SIGINT`, or `SIGHUP`. Process termination is handled exclusively through `terminate`.
4. **Inspect full output**: Call `read_exec_log` with `execution_id` and stream (`stdout` or `stderr`) to page through log files when output was truncated.

Finished processes stay tracked, so `status` and repeated `wait` calls keep returning the same result.

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
