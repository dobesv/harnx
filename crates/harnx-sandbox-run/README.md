# harnx-sandbox-run

[See the full documentation](../../docs/sandbox-run.md) for more examples and details.

`harnx-sandbox-run` is a standalone utility to run arbitrary commands inside the [birdcage](https://github.com/phylum-dev/birdcage) sandbox. It provides the same sandboxing defaults and configuration as `harnx-mcp-bash` but can be used as a general-purpose wrapper for any CLI tool.

It also supports **hooks** for credential injection, allowing you to securely provide environment variables or files to the sandboxed process without exposing them to the host environment permanently.

## Installation

```bash
cargo install harnx-sandbox-run
```

## Usage Examples

### Basic usage
Run an interactive bash session in the sandbox:
```bash
harnx-sandbox-run -- bash
```

### AI Agent Wrappers (Primary Use Case)

Running AI coding agents safely with permissions bypassed (the sandbox provides the actual safety boundary):

#### Claude Code (`claude-sb`)
```bash
#!/usr/bin/env bash
exec harnx-sandbox-run \
  --hook claude-command-persistent harnx-aws-creds \; \
  -- \
  claude --dangerously-skip-permissions "$@"
```

#### Gemini CLI (`gemini-sb`)
```bash
#!/usr/bin/env bash
exec harnx-sandbox-run \
  -- \
  gemini --yolo "$@"
```

### With AWS credential hooks
Uses `harnx-aws-creds` to inject AWS credentials before running `claude`:
```bash
harnx-sandbox-run --hook claude-command-persistent harnx-aws-creds --profile my-profile \; -- claude
```

### Granting extra permissions
Give the sandbox full RWX access to a specific directory. Use `.` to grant access to the current directory:
```bash
harnx-sandbox-run --extra-rwx . --extra-rwx ~/.npm -- npm install
```

If `.` resolves to `$HOME` or an ancestor, `harnx-sandbox-run` prints a warning and ignores it rather than exposing your entire home directory.

### Disabling network
```bash
harnx-sandbox-run --no-network -- curl google.com
```

## CLI Reference

```text
Usage: harnx-sandbox-run [OPTIONS] -- <COMMAND>...

Arguments:
  <COMMAND>...  Command to run (required, must come after `--` or at end)

Options:
      --extra-read <path>          Add sandbox read-only path (may be repeated)
      --extra-write <path>         Add sandbox writable path (may be repeated)
      --extra-exec <path>          Add sandbox execute path (may be repeated)
      --extra-rwx <path>           Add sandbox read/write/exec path (may be repeated)
      --env <VAR[=VALUE]>          Set environment variable (VAR=VALUE); if VALUE omitted, inherit from host
      --no-network                 Disable network access
      --working-dir <path>         Working directory for the command
      --no-defaults                Skip default whitelist (system paths, home paths, env-relative paths)
      --hook <TYPE> <CMD> [ARGS...] \;
                                   Pre-run hook for credential injection or env mutation
  -h, --help                       Print help

Environment:
  HARNX_BASH_EXTRA_READABLE   Colon-separated extra sandbox read-only paths
  HARNX_BASH_EXTRA_EXEC       Colon-separated extra sandbox execute paths
  HARNX_BASH_EXTRA_WRITABLE   Colon-separated extra sandbox writable paths
  HARNX_BASH_EXTRA_RWX        Colon-separated extra sandbox read/write/exec paths
  HARNX_BASH_ENV_PASSTHROUGH  Comma-separated extra env var names to pass through
```

## How Hooks Work

Hooks allow you to run a command before the main sandboxed process starts. These hooks can mutate the environment variables that will be passed to the sandboxed process.

Syntax: `--hook TYPE CMD ARGS... \;`

- **TYPE**: Currently supports `claude-command` (one-shot) or `claude-command-persistent`.
- **CMD ARGS...**: The command to execute and its arguments.
- **`\;`**: Terminates the hook definition (can also be a plain `;` if escaped from the shell).

`harnx-sandbox-run` sends a `PreToolUse` event to each hook command before spawning the sandboxed process. Hook commands respond by returning environment-variable mutations, which `harnx-sandbox-run` collects and applies to the environment of the spawned process.

## Platform Support

`harnx-sandbox-run` relies on `birdcage` for sandboxing, which currently supports **Unix-like** systems:
- Linux
- macOS

## Relationship to `harnx-mcp-bash`

This binary uses the shared `harnx-sandbox-common` crate, ensuring that it uses the exact same default sandbox policies (whitelisted system paths, etc.) as the `harnx-mcp-bash` MCP server. It is essentially the standalone, non-MCP version of the same sandboxing logic.
