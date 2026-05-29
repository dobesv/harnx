# harnx-sandbox-run

## Overview

`harnx-sandbox-run` is a standalone utility to run arbitrary commands inside the [birdcage](https://github.com/phylum-dev/birdcage) filesystem sandbox. It provides the same sandboxing defaults and configuration as `harnx-mcp-bash`, but as a general-purpose wrapper for any CLI tool.

This is particularly useful for running AI coding agents (like Claude Code or the Gemini CLI) safely. By running them inside the sandbox, you can bypass their internal permission prompts while maintaining a strict, verifiable safety boundary at the OS level.

## Installation

```bash
cargo install harnx-sandbox-run
```

## Basic Usage

Run an interactive bash session in the sandbox:
```bash
harnx-sandbox-run -- bash
```

Run a non-interactive command:
```bash
harnx-sandbox-run -- bash -c "echo hello"
```

## AI Agent Wrapper Scripts

The primary use case for `harnx-sandbox-run` is running AI coding agents with bypassed permissions. Since the sandbox provides the actual safety boundary (preventing writes outside the project, network calls to unexpected hosts, etc.), the agent's own interactive permission prompts become redundant.

### Claude Code (`claude-sb`)

Create a script named `claude-sb`. This example shows the full setup with AWS credentials, GitHub auth (git over HTTPS + API), and Atlassian (Jira/Confluence) — remove any sections you don't need:

```bash
#!/usr/bin/env bash
# claude-sb — run Claude Code inside the birdcage sandbox
#
# --dangerously-skip-permissions bypasses Claude's own permission prompts;
# the sandbox is the actual safety boundary.
#
# Credentials are injected via hooks — real tokens never enter the sandbox:
#   - harnx-aws-creds: resolves AWS credentials from the host and exposes
#     them via the container credentials protocol (no raw keys in env)
#   - harnx-proxy-auth: HTTPS MITM proxy that intercepts outbound requests
#     and injects auth headers; --env scripts put fake sentinel tokens into
#     the sandbox env so the LLM never sees real credentials
exec harnx-sandbox-run \
  \
  --extra-rwx ~/projects \
  \
  --extra-rwx ~/.claude \
  --extra-rwx ~/.local/share/claude \
  --extra-write ~/.claude.json \
  \
  --env "EDITOR=true" \
  --env "PUPPETEER_NO_SANDBOX=1" \
  \
  --hook claude-command-persistent harnx-aws-creds --profile my-profile \; \
  \
  --hook claude-command-persistent harnx-proxy-auth \
    --env 'if env.GITHUB_TOKEN
        then .GITHUB_TOKEN = "ghp_\($fake_base64_key)"
        end' \
    --hook 'if .host == "github.com"
            and .headers.authorization == basic("x-access-token"; "ghp_\($fake_base64_key)")
        then .headers.authorization = basic("x-access-token"; env.GITHUB_TOKEN)
        end' \
    --hook 'if (.host == "api.github.com"
                or .host == "uploads.github.com"
                or .host == "objects.githubusercontent.com")
            and (.headers.authorization == bearer("ghp_\($fake_base64_key)")
                 or .headers.authorization == "token ghp_\($fake_base64_key)")
        then .headers.authorization = bearer(env.GITHUB_TOKEN)
        end' \
    --env 'if env.ATLASSIAN_API_TOKEN and env.ATLASSIAN_EMAIL
        then .ATLASSIAN_API_TOKEN = $fake_uuid_key | .ATLASSIAN_EMAIL = $fake_email
        end' \
    --hook 'if (.host | endswith(".atlassian.net"))
            and .headers.authorization == basic($fake_email; $fake_uuid_key)
        then .headers.authorization = basic(env.ATLASSIAN_EMAIL; env.ATLASSIAN_API_TOKEN)
        end' \
  \; \
  \
  -- \
  claude --dangerously-skip-permissions "$@"
```

**Key points:**
- Replace `~/projects` with a folder where you have files the agent needs to work on, and `my-profile` with your AWS profile name (omit `--hook ... harnx-aws-creds ...` entirely if you don't need AWS).
- The `--env` scripts inject fake sentinel tokens (e.g. `ghp_<random>`) into the sandbox so `gh` sees `GITHUB_TOKEN` set and considers itself authenticated. The real token never enters the sandbox — `harnx-proxy-auth` reads it from the host env and injects it into outbound HTTP headers.
- Remove the Atlassian block if you don't use Jira/Confluence.
- `if COND then EXPR end` — omitting `else` passes the input through unchanged (jaq default).

### Gemini CLI (`gemini-sb`)

Create a script named `gemini-sb`:

```bash
#!/usr/bin/env bash
# gemini-sb — run Gemini CLI inside the birdcage sandbox
#
# --yolo bypasses Gemini's own permission prompts;
# the sandbox is the actual safety boundary.
#
# ~/.gemini        — global settings, OAuth credentials, commands, skills
# ~/.config/gemini — XDG config fallback (used on some systems)
# .                — the project directory you want the agent to work in
#                    (the sandbox does NOT whitelist the current directory
#                    automatically — you must grant access explicitly;
#                    passing . is safe: harnx-sandbox-run resolves it and
#                    refuses to grant access if it would expose $HOME)
exec harnx-sandbox-run \
  --extra-rwx ~/.gemini \
  --extra-rwx ~/.config/gemini \
  --extra-rwx . \
  -- \
  gemini --yolo "$@"
```

> **Note:** Pass `--extra-rwx` for each additional project directory you want Gemini to access. The sandbox restricts writes to only the paths you explicitly grant.

### Installation

1. Save the scripts to a directory in your `PATH` (e.g., `~/.local/bin/`).
2. Make them executable:
   ```bash
   chmod +x ~/.local/bin/claude-sb ~/.local/bin/gemini-sb
   ```

## CLI Reference

| Option | Description |
| :--- | :--- |
| `--extra-read <path>` | Add sandbox read-only path (may be repeated) |
| `--extra-write <path>` | Add sandbox writable path (may be repeated) |
| `--extra-exec <path>` | Add sandbox execute path (may be repeated) |
| `--extra-rwx <PATH>` | Add full rwx access for path (may be repeated) |
| `--env <VAR[=VALUE]>` | Set environment variable; if VALUE omitted, inherit from host |
| `--no-network` | Disable network access |
| `--working-dir <DIR>` | Working directory for the command |
| `--no-defaults` | Skip default whitelist (system paths, home paths, etc.) |
| `--hook <TYPE> <CMD> [ARGS...] \;` | Pre-run hook for credential injection |
| `-h, --help` | Print help |

## Environment Variables

These match the `harnx-mcp-bash` environment variables, so the same shell profile settings work for both:

| Variable | Format | Effect |
| :--- | :--- | :--- |
| `HARNX_BASH_EXTRA_READABLE` | Colon-separated paths | Extra sandbox read-only paths |
| `HARNX_BASH_EXTRA_EXEC` | Colon-separated paths | Extra sandbox execute paths |
| `HARNX_BASH_EXTRA_WRITABLE` | Colon-separated paths | Extra sandbox writable paths |
| `HARNX_BASH_EXTRA_RWX` | Colon-separated paths | Extra sandbox read/write/exec paths |
| `HARNX_BASH_ENV_PASSTHROUGH` | Comma-separated names | Extra host env var names to pass through |

Paths support `~` expansion. CLI flags and env vars both accumulate — they are not mutually exclusive.

> **Note:** The `<COMMAND>` must come after `--` or at the very end of the arguments.

## Default Whitelist

By default, `harnx-sandbox-run` grants access to a curated set of system paths and common developer tools (like `cargo`, `npm`, and `git`).

For the full list of default paths, see the [Bash MCP Server documentation](bash-mcp-server.md#default-environment-allowlist).

### Current directory

**The current working directory is NOT whitelisted automatically.** birdcage inherits the process's cwd so the command starts in the right place, but the sandbox blocks all reads and writes there unless you explicitly grant access:

```bash
harnx-sandbox-run --extra-rwx . -- my-tool
```

Passing `.` is safe: `harnx-sandbox-run` resolves it to an absolute path before use. If the resolved path is `$HOME` itself or an ancestor of `$HOME` (e.g. `/`), the path is silently skipped and a warning is printed to stderr — so running `gemini-sb` from your home directory won't accidentally expose all of `~`.

The `--working-dir <path>` flag changes where the sandboxed command starts, but it also does not automatically grant filesystem access to that path — you still need a matching `--extra-rwx` (or `--extra-read` / `--extra-write`) for it.

### Home directory

**`$HOME` itself is NOT whitelisted.** Only specific subdirectories are granted access by default, covering common developer toolchains and dotfiles:

| Access | Paths |
| :----- | :---- |
| Read | `~/.gitconfig`, `~/.gitignore`, `~/.gitignore_global`, `~/.tool-versions` |
| Read/Write | `~/.cache`, `~/go/pkg` |
| Read/Write/Exec | `~/.npm`, `~/.yarn`, `~/.nvm`, `~/.cargo`, `~/.pyenv`, `~/.rye`, `~/.bun`, `~/.local/share/claude`, `~/.local/share/uv`, `~/.local/share/pnpm`, `~/.local/share/pipx`, `~/.local/share/opencode` |
| Exec | `~/.local/bin`, `~/.local/lib`, `~/.asdf`, `~/.bun`, `~/go/bin` |

Any other `$HOME` subdirectory (e.g. `~/.gemini`, `~/.config`, `~/.ssh`) is **blocked** unless you add it with `--extra-read`, `--extra-write`, or `--extra-rwx`. This is intentional — it prevents the sandboxed process from reading credentials or config files it doesn't need.

## Hooks

Hooks allow you to run a command before the main process starts to inject credentials or mutate the environment.

**Syntax:** `--hook TYPE CMD ARGS... \;`

- **TYPE**: Currently supports `claude-command` (one-shot) or `claude-command-persistent`.
- **CMD ARGS...**: The command to execute and its arguments.
- **`\;`**: Terminates the hook definition (the backslash is required in most shells to prevent the `;` from being interpreted as a command separator).

### Example: AWS Credentials with `harnx-aws-creds`

`harnx-aws-creds` is a purpose-built persistent hook that makes AWS credentials available inside the sandbox without mounting `~/.aws` or exposing raw key material as plain environment variables.

**How it works:**

1. On startup, it resolves credentials from the host using the standard AWS credential chain (env vars → `~/.aws/credentials` → IAM instance role → SSO → etc.)
2. It starts a local HTTP server on `127.0.0.1:<random-port>` implementing the [AWS container credentials protocol](https://docs.aws.amazon.com/sdkref/latest/guide/feature-container-credentials.html)
3. For every `bash_exec`/`bash_spawn` tool call it injects three env vars into the sandboxed process:
   - `AWS_CONTAINER_CREDENTIALS_FULL_URI` — points to the local server
   - `AWS_CONTAINER_AUTHORIZATION_TOKEN` — a per-session bearer token
   - `AWS_REGION` — resolved from config

The AWS SDK inside the sandbox calls back to the local server to fetch credentials on demand. The sandbox process never sees `~/.aws` or raw `AWS_ACCESS_KEY_ID` values. See [harnx-aws-creds documentation](aws-creds.md) for full details.

```bash
# Use the default credential chain (env vars, ~/.aws default profile, IAM role, etc.)
harnx-sandbox-run --hook claude-command-persistent harnx-aws-creds \; -- claude

# Use a specific named profile from ~/.aws/config
harnx-sandbox-run --hook claude-command-persistent harnx-aws-creds --profile my-profile \; -- claude
```

### Example: GitHub credentials with `harnx-proxy-auth`

The sandbox strips `GITHUB_TOKEN` from the child environment by default. `harnx-proxy-auth` is an HTTPS MITM proxy hook that intercepts outbound requests and injects auth headers, so the real token never has to enter the sandbox directly.

**How it works:**

1. An `--env` script runs on startup, reads `GITHUB_TOKEN` from the host env, and injects a fake sentinel token (e.g. `ghp_<random>`) into the sandbox's environment. Tools like `gh` see a non-empty `GITHUB_TOKEN` and consider themselves authenticated.
2. `--hook` jaq filters run on every outbound HTTPS request. When they see a request carrying the sentinel token, they replace it with `bearer(env.GITHUB_TOKEN)` (the real token, read from the host-side proxy process).

The real token never crosses into the sandbox — it only appears in HTTP headers on the wire.

Available jaq helpers: `bearer(token)` → `"Bearer <token>"`, `basic(user; pass)` → `"Basic <base64(user:pass)>"`.

```bash
harnx-sandbox-run \
  --hook claude-command-persistent harnx-proxy-auth \
    --env 'if env.GITHUB_TOKEN
        then .GITHUB_TOKEN = "ghp_\($fake_base64_key)"
        end' \
    --hook 'if .host == "github.com"
            and .headers.authorization == basic("x-access-token"; "ghp_\($fake_base64_key)")
        then .headers.authorization = basic("x-access-token"; env.GITHUB_TOKEN)
        end' \
    --hook 'if (.host == "api.github.com"
                or .host == "uploads.github.com"
                or .host == "objects.githubusercontent.com")
            and (.headers.authorization == bearer("ghp_\($fake_base64_key)")
                 or .headers.authorization == "token ghp_\($fake_base64_key)")
        then .headers.authorization = bearer(env.GITHUB_TOKEN)
        end' \
  \; \
  -- gh api user
```

Three hooks are needed because GitHub uses different auth schemes per endpoint:
- `github.com` — git over HTTPS uses `Basic x-access-token:<token>`
- `api.github.com`, `uploads.github.com`, `objects.githubusercontent.com` — REST/GraphQL APIs use `Bearer <token>` (curl, SDKs) or `token <token>` (`gh` CLI)

The hooks match the *exact* sentinel values injected by `--env` — `basic("x-access-token"; "ghp_\($fake_base64_key)")`, `bearer("ghp_\($fake_base64_key)")`, and `"token ghp_\($fake_base64_key)"` — so they only fire on requests carrying the fake credential, never on a real token the user might have set independently.

`if COND then EXPR end` — omitting `else` passes the request through unchanged when the condition is false.

### Example: Atlassian (Jira / Confluence) with `harnx-proxy-auth`

Atlassian's REST API uses HTTP Basic auth with your email as the username and an API token as the password.

```bash
harnx-sandbox-run \
  --hook claude-command-persistent harnx-proxy-auth \
    --env 'if env.ATLASSIAN_API_TOKEN and env.ATLASSIAN_EMAIL
        then .ATLASSIAN_API_TOKEN = $fake_uuid_key | .ATLASSIAN_EMAIL = $fake_email
        end' \
    --hook 'if (.host | endswith(".atlassian.net"))
            and .headers.authorization == basic($fake_email; $fake_uuid_key)
        then .headers.authorization = basic(env.ATLASSIAN_EMAIL; env.ATLASSIAN_API_TOKEN)
        end' \
  \; \
  -- acli jira workitem search --jql "assignee = currentUser() AND resolution = Unresolved"
```

The `--env` script injects fake sentinel values (`$fake_uuid_key` for the token, `$fake_email` for the email) into the sandbox. The `--hook` filter matches requests carrying those sentinels and replaces them with the real credentials from the host env. Set `ATLASSIAN_EMAIL` and `ATLASSIAN_API_TOKEN` in your host shell before running.

### Combining multiple hooks in one `harnx-proxy-auth` invocation

All `--env` and `--hook` flags for a single `harnx-proxy-auth` instance accumulate — you don't need separate hook invocations for GitHub and Atlassian:

```bash
harnx-sandbox-run \
  --hook claude-command-persistent harnx-proxy-auth \
    --env 'if env.GITHUB_TOKEN
        then .GITHUB_TOKEN = "ghp_\($fake_base64_key)"
        end' \
    --hook 'if (.host == "api.github.com" or .host == "uploads.github.com" or .host == "objects.githubusercontent.com")
            and (.headers.authorization == bearer("ghp_\($fake_base64_key)")
                 or .headers.authorization == "token ghp_\($fake_base64_key)")
        then .headers.authorization = bearer(env.GITHUB_TOKEN)
        end' \
    --hook 'if .host == "github.com"
            and .headers.authorization == basic("x-access-token"; "ghp_\($fake_base64_key)")
        then .headers.authorization = basic("x-access-token"; env.GITHUB_TOKEN)
        end' \
    --env 'if env.ATLASSIAN_API_TOKEN and env.ATLASSIAN_EMAIL
        then .ATLASSIAN_API_TOKEN = $fake_uuid_key | .ATLASSIAN_EMAIL = $fake_email
        end' \
    --hook 'if (.host | endswith(".atlassian.net"))
            and .headers.authorization == basic($fake_email; $fake_uuid_key)
        then .headers.authorization = basic(env.ATLASSIAN_EMAIL; env.ATLASSIAN_API_TOKEN)
        end' \
  \; \
  -- my-tool
```

See the `claude-sb` wrapper script above for a complete working example combining AWS credentials, GitHub, and Atlassian in one invocation.

## Extra Path Access

If a tool needs access to paths not in the default whitelist, use the access flags:

- `--extra-read`: Read-only access (e.g., for config files)
- `--extra-write`: Write access (e.g., for log files)
- `--extra-exec`: Execution access (e.g., for binaries)
- `--extra-rwx`: Full read, write, and execute access

```bash
harnx-sandbox-run --extra-rwx ~/.custom-tool-cache -- my-tool
```

## Network Access

Network access is enabled by default. To run a command in a fully network-isolated environment:

```bash
harnx-sandbox-run --no-network -- cargo test
```

## Platform Support

`harnx-sandbox-run` relies on `birdcage`, which currently supports:
- **Linux**
- **macOS**

Windows is currently not supported for filesystem sandboxing.
