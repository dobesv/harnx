# harnx-sandbox-run

## Overview

`harnx-sandbox-run` is a standalone utility to run arbitrary commands inside the [birdcage](https://github.com/phylum-dev/birdcage) filesystem sandbox. It provides the same sandboxing defaults and configuration as `harnx-bash-tools`, but as a general-purpose wrapper for any CLI tool.

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

You can transparently sandbox your existing tools with a "shim directory." You place scripts named after the real commands (`claude`, `gemini`, `node`, `yarn`, …) in a directory that is first on your `PATH`, so typing the normal command runs it inside the sandbox — no need to remember a special wrapper name.

### Shim directory setup

```bash
# Create the shim directory
mkdir -p "${XDG_DATA_HOME:-$HOME/.local/share}/harnx/sandbox-bin"
```

Prepend this directory to your `PATH` in your shell profile (e.g., `~/.bashrc`, `~/.zshrc`):

```bash
export PATH="${XDG_DATA_HOME:-$HOME/.local/share}/harnx/sandbox-bin:$PATH"
```

The shim directory must be **first** on your `PATH` so the shims shadow the real tools. For the installation commands below, we'll use a `SHIM_DIR` variable:

```bash
SHIM_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/harnx/sandbox-bin"
```

### How the shims work

Each shim script identifies its own location and strips that directory from the `PATH` before executing the real tool inside the sandbox. This is necessary because `harnx-sandbox-run` passes your `PATH` through to the sandboxed process, and `birdcage` resolves the target command against that `PATH`. If the shim directory remained first, the shim would re-invoke itself and recurse on the host. Stripping the directory (and the fact that the shim directory is not on the sandbox's execution whitelist) prevents this.

For project access, the shims use [Project-Root Pseudo-Variables](#project-root-pseudo-variables) like `$GIT_ROOT` to automatically grant access to the current project's roots. These are silently skipped if you are not in a matching project.

### Node, Yarn, npm, npx, pnpm (`node`)

A single dispatcher script can handle the entire Node family by detecting which name it was called with. Common Node caches are already whitelisted by default, and project roots are granted automatically.

> **`npx` note:** the default whitelist grants `~/.npm` read+write but **not execute** (a writable+executable directory lets sandboxed code plant a binary the host might later run). `npx <pkg>@<version>` installs the package under `~/.npm/_npx/<hash>/node_modules/` and then executes its bin from there, so without an exec grant it fails with `sh: 1: <pkg>: Permission denied`. The shim below opts in with `--allow-rwx ~/.npm`. Omit that line if you never use `npx` to run cached package binaries.
```bash
#!/usr/bin/env bash
set -euo pipefail

# Node-family sandbox shim.
# Save this file as "node", then symlink or copy it to yarn, npm, npx, and pnpm.
# It detects which real tool to run from its own basename, so one script covers all five.
# Common Node caches are already in harnx-sandbox-run's default whitelist.
# Exception: ~/.npm is read+write but NOT exec by default, so `npx <pkg>@<ver>`
# cannot run its cached bin. We add --allow-rwx ~/.npm below to allow it.
# Project roots are auto-detected by harnx-sandbox-run's pseudo-vars and silently skipped when absent.

tool="$(basename "$0")"
self_dir="$(cd "$(dirname "$0")" && pwd -P)"
PATH="$({
  old_ifs=$IFS
  IFS=:
  for path_entry in $PATH; do
    if [ "$path_entry" != "$self_dir" ]; then
      if [ -n "${new_path-}" ]; then
        new_path="${new_path}:$path_entry"
      else
        new_path="$path_entry"
      fi
    fi
  done
  IFS=$old_ifs
  printf '%s' "${new_path-}"
})" # Drop shim dir from PATH by exact element match to prevent harnx-sandbox-run -- "$tool" from resolving back to shim and recursing.
export PATH

# shellcheck disable=SC2016 -- '$GIT_ROOT' style args are harnx-sandbox-run pseudo-vars; shell must pass them through literally.
# Common tool and cache paths (like ~/.npm, ~/.cargo, ~/.cache) are pre-whitelisted
# for read/write; ~/.npm needs the explicit --allow-rwx below to also allow exec (see npx note above).
exec harnx-sandbox-run \
  --allow-rwx ~/.npm \
  --allow-rwx '$GIT_ROOT' \
  --allow-rwx '$NODE_PROJECT_ROOT' \
  --allow-rwx '$GIT_COMMON_DIR' \
  --env 'AWS_PROFILE=' \
  --env NODE_OPTIONS \
  --hook claude-command-persistent harnx-aws-creds --profile my-profile \; \
  -- "$tool" "$@"
```

AWS access:

Be sure to set the appropriate AWS profile name for `harnx-aws-creds` if your dev tools/servers need AWS access (e.g. for build caching
or other S3 storage), otherwise remove that line if it is not needed.

Installation:

```bash
SHIM_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/harnx/sandbox-bin"
# Save the script above as "$SHIM_DIR/node", then:
chmod +x "$SHIM_DIR/node"
for t in yarn npm npx pnpm; do ln -sf node "$SHIM_DIR/$t"; done
```

### Claude Code (`claude`)

Claude Code requires access to its own configuration and credentials, along with credential hooks for AWS, GitHub, and Atlassian.
```bash
#!/usr/bin/env bash
set -euo pipefail

# claude — run Claude Code inside birdcage sandbox
#
# --dangerously-skip-permissions bypasses Claude's own permission prompts;
# sandbox is actual safety boundary.
#
# Credentials are injected via hooks — real tokens never enter sandbox:
#   - harnx-aws-creds: resolves AWS credentials from host and exposes
#     them via container credentials protocol (no raw keys in env)
#   - harnx-proxy-auth: HTTPS MITM proxy that intercepts outbound requests
#     and injects auth headers; --env scripts put fake sentinel tokens into
#     sandbox env so LLM never sees real credentials

self_dir="$(cd "$(dirname "$0")" && pwd -P)"
PATH="$({
  old_ifs=$IFS
  IFS=:
  for path_entry in $PATH; do
    if [ "$path_entry" != "$self_dir" ]; then
      if [ -n "${new_path-}" ]; then
        new_path="${new_path}:$path_entry"
      else
        new_path="$path_entry"
      fi
    fi
  done
  IFS=$old_ifs
  printf '%s' "${new_path-}"
})" # Drop shim dir from PATH by exact element match to prevent harnx-sandbox-run -- claude from resolving back to shim and recursing.
export PATH

# shellcheck disable=SC2016 -- '$GIT_ROOT' style args are harnx-sandbox-run pseudo-vars; shell must pass them through literally.
# ~/.local/share/claude is already default-whitelisted, so intentionally not listed here.
exec harnx-sandbox-run \
  --allow-rwx '$GIT_ROOT' \
  --allow-rwx '$NODE_PROJECT_ROOT' \
  --allow-rwx '$GIT_COMMON_DIR' \
  --allow-rwx ~/.claude \
  --allow-write ~/.claude.json \
  --env "EDITOR=true" \
  --env "PUPPETEER_NO_SANDBOX=1" \
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
    --fs '{"harnx-fs-acli/acli/.keep": ""}' \
    --env '{"ACLI_CONFIG_DIR": "\($temp_file_root)/harnx-fs-acli"}' \
    --hook "$self_dir/jira-auth-hook.py" \
  \; \
  -- claude --dangerously-skip-permissions "$@"
```
**Key points:**
- Replace `my-profile` with your AWS profile name (omit `--hook ... harnx-aws-creds ...` entirely if you don't need AWS). Project access now uses harnx-sandbox-run pseudo-vars, so current git root / node project root / git common dir are granted automatically when present; add `--allow-rwx /path` for any extra directories the agent should access.
- The `--env` and `--fs` scripts inject fake sentinel tokens into the sandbox (e.g. `ghp_<random>` for GitHub, or a synthetic `jira_config.yaml` for Atlassian) so tools like `gh` or `acli` consider themselves authenticated. The real tokens never enter the sandbox — `harnx-proxy-auth` reads them from the host (environment or OS keyring) and injects them into outbound HTTP headers.
- The Atlassian flow is handled by the **`jira-auth-hook.py`** script (referenced by path in the last `--hook` — here `"$self_dir/jira-auth-hook.py"`, a sibling of this launcher in the shim dir; see [the Atlassian section](#example-atlassian-jira--confluence-with-harnx-proxy-auth) for a one-line download command). It reads your host `acli` profile, sources the real token from the OS keyring, writes a synthetic `jira_config.yaml` (holding only a sentinel token) into the proxy's per-run temp dir, and rewrites auth for `api.atlassian.com` + your site on the wire. The `--fs`/`--env` lines just allocate that temp dir and point `ACLI_CONFIG_DIR` at it. **It works only when `acli` is authenticated with an API token, not OAuth.** As long as you have logged in with an API token on your host, it works automatically with no manual environment variables required.
- The keyring lookup is platform-aware: Linux Secret Service (`secret-tool lookup service acli username jira:<profile>`) or the macOS login keychain (`security find-generic-password -s acli -a jira:<profile> -w`), selected automatically. Export `HARNX_JIRA_TOKEN_CMD` to override it for a different secret store.
- `acli jira auth status` will report "Unauthorized" inside the sandbox — that is expected. It is a local token-validation check that never hits the network, so it fails against the sentinel token; actual data commands (`acli jira workitem view`, `project list`, …) still work via the on-the-wire token swap. Judge success by whether data commands work.
- Remove the Atlassian block if you don't use Jira/Confluence.
- `if COND then EXPR end` — omitting `else` passes the input through unchanged (jaq default).

### Gemini CLI (`gemini`)
```bash
#!/usr/bin/env bash
set -euo pipefail

# gemini — run Gemini CLI inside birdcage sandbox
#
# --yolo bypasses Gemini's own permission prompts;
# sandbox is actual safety boundary.
#
# ~/.gemini        — global settings, OAuth credentials, commands, skills
# ~/.config/gemini — XDG config fallback (used on some systems)
# Project roots are auto-detected via $GIT_ROOT / $NODE_PROJECT_ROOT /
# $GIT_COMMON_DIR and silently skipped when absent.

self_dir="$(cd "$(dirname "$0")" && pwd -P)"
PATH="$({
  old_ifs=$IFS
  IFS=:
  for path_entry in $PATH; do
    if [ "$path_entry" != "$self_dir" ]; then
      if [ -n "${new_path-}" ]; then
        new_path="${new_path}:$path_entry"
      else
        new_path="$path_entry"
      fi
    fi
  done
  IFS=$old_ifs
  printf '%s' "${new_path-}"
})" # Drop shim dir from PATH by exact element match to prevent harnx-sandbox-run -- gemini from resolving back to shim and recursing.
export PATH

# shellcheck disable=SC2016 -- '$GIT_ROOT' style args are harnx-sandbox-run pseudo-vars; shell must pass them through literally.
exec harnx-sandbox-run \
  --allow-rwx ~/.gemini \
  --allow-rwx ~/.config/gemini \
  --allow-rwx '$GIT_ROOT' \
  --allow-rwx '$NODE_PROJECT_ROOT' \
  --allow-rwx '$GIT_COMMON_DIR' \
  -- gemini --yolo "$@"
```
If the agent needs access to additional directories, grant them with `--allow-rwx /path/to/dir` in the shim script.

### Installing the shims

Make all shims executable and verify that the shims appear first on your `PATH`:

```bash
chmod +x "$SHIM_DIR"/*
command -v node      # should print the shim path under sandbox-bin
which -a node         # confirm the shim appears before the real tool
```

## CLI Reference

| Option | Description |
| :--- | :--- |
| `--allow-read <path>` | Allow sandbox read-only path (may be repeated) |
| `--allow-write <path>` | Allow sandbox writable path (may be repeated) |
| `--allow-exec <path>` | Allow sandbox execute path (may be repeated) |
| `--allow-rwx <PATH>` | Add full rwx access for path (may be repeated) |
| `--env <VAR[=VALUE]>` | Set environment variable; if VALUE omitted, inherit from host |
| `--no-network` | Disable network access |
| `--working-dir <DIR>` | Working directory for the command |
| `--no-defaults` | Skip default whitelist (system paths, home paths, etc.) |
| `--hook <TYPE> <CMD> [ARGS...] \;` | Pre-run hook for credential injection |
| `-h, --help` | Print help |

## Environment Variables

These match the `harnx-bash-tools` environment variables, so the same shell profile settings work for both:

| Variable | Format | Effect |
| :--- | :--- | :--- |
| `HARNX_TOOLS_ALLOW_READ` | Colon-separated paths | Allowed sandbox read-only paths |
| `HARNX_TOOLS_ALLOW_EXEC` | Colon-separated paths | Allowed sandbox execute paths |
| `HARNX_TOOLS_ALLOW_WRITE` | Colon-separated paths | Allowed sandbox writable paths |
| `HARNX_TOOLS_ALLOW_RWX` | Colon-separated paths | Allowed sandbox read/write/exec paths |
| `HARNX_BASH_ENV_PASSTHROUGH` | Comma-separated names | Extra host env var names to pass through |

Paths support `~` expansion and project-root pseudo-variables. CLI flags and env vars both accumulate — they are not mutually exclusive.

> **MCP servers that need a static API key.** When an MCP server is launched through a sandbox-wrapped shim (e.g. `npx exa-mcp-server`), its environment is scrubbed like any other sandboxed process — so a key set in `~/.local/share/harnx/.env`, or in the server's `env:` block, will not reach it. Unlike the GitHub/Atlassian token flows below (where the real secret is kept out of the sandbox and injected into outbound requests by a proxy hook), a server that authenticates by sending the key itself just needs the value forwarded: add `HARNX_BASH_ENV_PASSTHROUGH=EXA_API_KEY` (or `--env EXA_API_KEY`). See the [MCP server config guide](configuration-guide.md#tool-servers-tool_servers).

> **Note:** The `<COMMAND>` must come after `--` or at the very end of the arguments.

## Default Whitelist

By default, `harnx-sandbox-run` grants access to a curated set of system paths and common developer tools (like `cargo`, `npm`, and `git`).

For the full list of default paths, see the [Bash toolset server documentation](bash-mcp-server.md#default-environment-allowlist).

### Current directory

**The current working directory is NOT whitelisted automatically.** birdcage inherits the process's cwd so the command starts in the right place, but the sandbox blocks all reads and writes there unless you explicitly grant access:

```bash
harnx-sandbox-run --allow-rwx . -- my-tool
```

Passing `.` is safe: `harnx-sandbox-run` resolves it to an absolute path before use. If the resolved path is `$HOME` itself or an ancestor of `$HOME` (e.g. `/`), the path is silently skipped and a warning is printed to stderr — so running the `gemini` shim from your home directory won't accidentally expose all of `~`.

The `--working-dir <path>` flag changes where the sandboxed command starts, but it also does not automatically grant filesystem access to that path — you still need a matching `--allow-rwx` (or `--allow-read` / `--allow-write`) for it.

### Home directory

**`$HOME` itself is NOT whitelisted.** Only specific subdirectories are granted access by default, covering common developer toolchains and dotfiles:

| Access | Paths |
| :----- | :---- |
| Read | `~/.gitconfig`, `~/.gitignore`, `~/.gitignore_global`, `~/.tool-versions` |
| Read/Write | `~/.cache`, `~/go/pkg`, `~/.npm`, `~/.yarn`, `~/.cargo/registry`, `~/.cargo/git`, `~/.bun/install/cache`, `~/.local/share/pnpm`, `~/.local/share/uv` |
| Exec | `~/.local/bin`, `~/.local/lib`, `~/.bun`, `~/.asdf`, `~/go/bin`, `~/.cargo`, `~/.nvm`, `~/.cargo/bin`, `~/.mono`, `~/.pyenv`, `~/.rye`, `~/.local/share/claude`, `~/.local/share/opencode`, `~/.local/share/pipx` |

Tool-install and self-update operations (such as `cargo install`, `nvm install`, etc.) require explicit write access; grant with `--allow-rwx` or perform them outside the sandbox.

Note that the Read/Write caches above are **not** executable. In particular, `npx <pkg>@<version>` installs and then runs a package binary from `~/.npm/_npx/...`, which fails with `sh: 1: <pkg>: Permission denied` unless you also grant exec. Add `--allow-rwx ~/.npm` (see the [Node shim](#node-yarn-npm-npx-pnpm-node) for an example).

Any other `$HOME` subdirectory (e.g. `~/.gemini`, `~/.config`, `~/.ssh`) is **blocked** unless you add it with `--allow-read`, `--allow-write`, or `--allow-rwx`. This is intentional — it prevents the sandboxed process from reading credentials or config files it doesn't need.

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

> **API-token auth only — OAuth is not supported.** This flow replays your stored credential as an HTTP Basic auth password, so `acli` must be logged in with an **API token**. It does **not** work if you authenticated with OAuth (`acli jira auth login --web`): OAuth stores a short-lived, rotating bearer token as a compressed binary blob, which the proxy can neither read (it is not valid UTF-8) nor replay as Basic auth — sandboxed `acli` then fails with `unauthorized: use 'acli jira auth login' to authenticate`. If `acli jira auth status` reports `Authentication Type: oauth`, switch to an API token:
>
> ```sh
> acli jira auth logout
> echo '<your-api-token>' | acli jira auth login --site "<your-site>.atlassian.net" --email "<your-email>" --token
> ```
>
> Create an API token at <https://id.atlassian.com/manage-profile/security/api-tokens>.

Atlassian's REST API uses HTTP Basic auth with your email as the username and an API token as the password.

**Setup — install the auth hook script.** The Atlassian flow is driven by `jira-auth-hook.py`. If you use the `harnx` **pantheon** package it ships at `~/.config/harnx/packages/pantheon/hooks/jira-auth-hook.py` and you can point `--hook` there. Otherwise (e.g. you only use `harnx-sandbox-run` standalone), download it once into your shim dir so the examples' `"$SHIM_DIR/jira-auth-hook.py"` path resolves:

```bash
SHIM_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/harnx/sandbox-bin"
mkdir -p "$SHIM_DIR"
curl -fsSL https://raw.githubusercontent.com/dobesv/harnx/main/example_config/jira-auth-hook.py \
  -o "$SHIM_DIR/jira-auth-hook.py"
chmod +x "$SHIM_DIR/jira-auth-hook.py"
```

(The script needs no execute bit to be used as a `--hook`, but `chmod +x` lets you run the sibling `probe-auth-hook.py` diagnostic directly.) In the examples below, `SHIM_DIR` must be set in your shell (or replace `"$SHIM_DIR/jira-auth-hook.py"` with the absolute path you saved it to).

```bash
harnx-sandbox-run \
  --hook claude-command-persistent harnx-proxy-auth \
    --fs '{"harnx-fs-acli/acli/.keep": ""}' \
    --env '{"ACLI_CONFIG_DIR": "\($temp_file_root)/harnx-fs-acli"}' \
    --hook "$SHIM_DIR/jira-auth-hook.py" \
  \; \
  -- acli jira workitem search --jql "assignee = currentUser() AND resolution = Unresolved"
```

The bundled **`jira-auth-hook.py`** script does the work: it reads your host `acli` profile, sources the real token from the OS keyring, writes a synthetic `jira_config.yaml` (holding only a sentinel token, written as a YAML `!!binary` scalar so `acli` accepts it) into the proxy's per-run temp dir, and replaces the sentinel with the real token on outbound `api.atlassian.com` / site requests. The `--fs`/`--env` lines allocate that temp dir and point `ACLI_CONFIG_DIR` at it. No manual environment variables are required as long as you have logged in with an **API token** (`acli jira auth login --site … --email … --token`) on your host. OAuth logins (`--web`) are not supported — see the warning above. The script sources the token from the platform keyring automatically: `secret-tool` on Linux and `security find-generic-password` on macOS. Override the lookup with `HARNX_JIRA_TOKEN_CMD` if you use a different secret store. Note that `acli jira auth status` reports "Unauthorized" inside the sandbox by design — it is a local-only token check; data commands still work.

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
    --fs '{"harnx-fs-acli/acli/.keep": ""}' \
    --env '{"ACLI_CONFIG_DIR": "\($temp_file_root)/harnx-fs-acli"}' \
    --hook "$SHIM_DIR/jira-auth-hook.py" \
  \; \
  -- my-tool
```

See the `claude` shim in [AI Agent Wrapper Scripts](#ai-agent-wrapper-scripts) above for a complete working example combining AWS credentials, GitHub, and Atlassian in one invocation.

## Extra Path Access

If a tool needs access to paths not in the default whitelist, use the access flags:

- `--allow-read`: Read-only access (e.g., for config files)
- `--allow-write`: Write access (e.g., for log files)
- `--allow-exec`: Execution access (e.g., for binaries)
- `--allow-rwx`: Full read, write, and execute access

```bash
harnx-sandbox-run --allow-rwx ~/.custom-tool-cache -- my-tool
```

## Project-Root Pseudo-Variables

Sandbox path flags (`--allow-read/-write/-exec/-rwx`) and environment variables (`HARNX_TOOLS_ALLOW_{READ,WRITE,EXEC,RWX}`) accept project-root pseudo-variables. These are resolved at startup against the current working directory:

| Pseudo-variable | Resolves to |
| :--- | :--- |
| `$GIT_ROOT` | git worktree root (`gix discover` workdir) |
| `$GIT_COMMON_DIR` | primary worktree's `.git` data dir (handles linked worktrees) |
| `$NODE_PROJECT_ROOT` | highest ancestor containing `package.json` (workspace root) |
| `$CARGO_ROOT` | highest ancestor containing `Cargo.toml` (workspace root) |
| `$GO_ROOT` | nearest ancestor containing `go.mod` |

### Semantics

- **Silent skip**: If the current directory is not inside a matching project, the path is silently dropped. This allows you to set global environment variables like `HARNX_TOOLS_ALLOW_RWX='$GIT_ROOT'` that only take effect when you are actually in a git repository.
- **Security**: Any pseudo-variable that resolves to `$HOME` or an ancestor of `$HOME` is dropped. The sandbox never grants access to your entire home directory via root detection.
- **Prefix match**: A pseudo-variable only triggers on an exact prefix-boundary match. `$GIT_ROOT` or `$GIT_ROOT/subdir` will be expanded, but `$GIT_ROOTX` or `/foo/$GIT_ROOT` will be treated as literal strings.
- **No escaping**: There is no mechanism to escape these variables. A directory literally named `$GIT_ROOT` cannot be targeted.
- **Shell quoting**: Use single quotes in your shell (e.g., `'$GIT_ROOT'`) to prevent the shell from attempting to expand them as shell variables.
- **Unix-only**: Project-root detection is currently supported on Unix systems only. On other platforms, these strings are ignored.

### Linked Worktrees

`$GIT_COMMON_DIR` is particularly useful for git history in linked worktrees. It resolves to the primary worktree's `.git` data directory, which is required for many git operations to function correctly from a linked worktree.

```bash
harnx-sandbox-run --allow-rwx '$GIT_ROOT' --allow-rwx '$GIT_COMMON_DIR' -- yarn install
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
