# Hooks Guide

Hooks allow you to extend Harnx by running external commands at specific points during agent execution. They can be used for auditing, policy enforcement, environment injection, or even modifying the interaction between the agent and its tools.

## 1. Overview

Hooks are external programs or scripts that Harnx executes when specific events occur. Depending on the event and the hook configuration, a hook can:

*   **Observe**: Record events for logging or auditing without affecting execution.
*   **Block**: Prevent a tool from running or stop an agent turn.
*   **Ask**: Pause execution and request user confirmation.
*   **Mutate**: Modify tool arguments before execution or tool responses after execution.
*   **Resume**: Trigger an additional agent turn after the agent has stopped.

## 2. Configuration

Hooks are configured in the `hooks` section of the global `config.yaml` or within an agent's front-matter.

### Configuration Fields

| Field | Type | Description |
| :--- | :--- | :--- |
| `event` | string | **Required.** The event that triggers the hook (e.g., `PreToolUse`). |
| `type` | string | **Required.** The execution protocol: `claude-command` or `claude-command-persistent`. |
| `matcher` | string | **Optional.** A regex matched against the `tool_name` for tool-related events. |
| `command` | string | **Required.** The shell command or path to the executable to run. |
| `timeout` | integer | **Optional.** Execution timeout in seconds (default: 30). |
| `status_message` | string | **Optional.** A message displayed to the user while the hook is running. |
| `async` | boolean | **Optional.** If `true`, the hook runs in the background. Async hooks cannot block or mutate. |
| `max_resume` | integer | **Optional.** (Top-level only) Maximum number of times a `Stop` hook can request a resume. |

### Hook Location and Merging

Hooks can be defined globally in `config.yaml` or per-agent in the agent's front-matter.

*   **Global Hooks**: Apply to all agents and sessions.
*   **Agent Hooks**: Defined in an agent's YAML front-matter.
*   **Merging**: Agent hooks extend the global list. If an agent hook has the same `event` and `matcher` as a global hook, the agent hook **replaces** the global one.
*   **max_resume**: If set in an agent's front-matter, it overrides the global `max_resume` value.

## 3. Event Reference

Harnx supports the following events. Each event sends a JSON payload to the hook.

| Event | When it fires | Payload Fields | Capabilities |
| :--- | :--- | :--- | :--- |
| `SessionStart` | At the beginning of a session. | `session_id`, `cwd`, `source`, `model` | Observe |
| `SessionEnd` | When a session terminates. | `session_id`, `cwd`, `reason` | Observe |
| `UserPromptSubmit` | When the user sends a prompt. | `session_id`, `cwd`, `prompt` | Observe |
| `Stop` | When the agent finishes its turn. | `session_id`, `cwd`, `stop_hook_active`, `last_assistant_message` | Resume |
| `StopFailure` | When an agent turn fails. | `session_id`, `cwd`, `error`, `error_type` | Observe |
| `PreToolUse` | Before a tool is executed. | `session_id`, `cwd`, `tool_name`, `tool_input`, `tool_use_id` | Block, Ask, Mutate |
| `PostToolUse` | After a tool successfully runs. | `session_id`, `cwd`, `tool_name`, `tool_input`, `tool_response`, `tool_use_id` | Mutate |
| `PostToolUseFailure`| When a tool execution fails. | `session_id`, `cwd`, `tool_name`, `tool_input`, `tool_use_id`, `error` | Observe |

## 4. Hook Types

### `claude-command` (One-shot)

The most common hook type. Harnx spawns the command as a subprocess for every event match.
*   **Input**: The event payload is sent to the command's `stdin` as a single JSON object.
*   **Output**: Harnx reads `stdout` for a JSON response (see Protocol below).
*   **Control**:
    *   Exit code `0`: Continue execution.
    *   Exit code `2`: Block execution (equivalent to `permissionDecision: "deny"`).
    *   Other non-zero codes: Logged as errors, but execution usually continues.

### `claude-command-persistent` (Persistent)

Useful for hooks that need to maintain state or have high startup overhead. The process is started at the beginning of the session and kept alive.
*   **Protocol**: JSONL (JSON Lines) over `stdin` and `stdout`.
*   **Correlation**: Each request from Harnx includes a unique `id` field. The hook must include the same `id` in its response line.

### Async Hooks (`async: true`)

Any hook can be marked as `async`.
*   Harnx fires the hook and continues immediately without waiting for a response.
*   Async hooks **cannot** block or mutate tool data. Any such fields in their response are ignored.
*   They can still request a `resume` or provide `additionalContext`, which will be applied to the *next* agent turn.
*   Harnx ensures all async hooks have completed their execution before starting a new LLM turn.

## 5. Response Protocol

Hooks communicate their results by printing a JSON object to `stdout`.

```json
{
  "additionalContext": "Text to append to the agent's conversation history",
  "resume": false,
  "systemMessage": "A hidden message to inject into the system prompt for the next turn",
  "hookSpecificOutput": {
    "permissionDecision": "allow",
    "permissionDecisionReason": "Reason for the decision",
    "toolInput": { "arg": "mutated value" },
    "toolResponse": { "result": "mutated response" }
  }
}
```

### Field Details

*   **`additionalContext`**: String. Appended to the conversation context.
*   **`resume`**: Boolean. If `true`, requests that the agent perform another turn (primary use for `Stop` hooks).
*   **`systemMessage`**: String. Injected as a system-level message.
*   **`hookSpecificOutput`**:
    *   **`permissionDecision`**: One of `"allow"`, `"deny"`, or `"ask"`.
    *   **`permissionDecisionReason`**: Description of why access was allowed, denied, or why the user is being asked.
    *   **`toolInput`**: (PreToolUse only) An object that replaces the original arguments sent to the tool.
    *   **`toolResponse`**: (PostToolUse only) A value (object or string) that replaces the original response from the tool.

## 6. Mutation

Mutation allows hooks to transparently modify the data flowing between the agent and its tools.

### Chaining Semantics

When multiple hooks are configured for the same event (e.g., two `PreToolUse` hooks for `bash_exec`):
1.  Hooks fire in **document order** (global hooks first, then agent hooks).
2.  Each hook receives the **current** value in its payload. If a previous hook mutated the value, the subsequent hook sees the mutated version.
3.  The final value used by Harnx is the one produced by the **last** hook in the chain that provided a mutation.

### Mutation Example: Injecting Environment Variables

A `PreToolUse` hook can inject secrets or configuration into a `bash_exec` call.

**hook-script.sh:**
```bash
#!/bin/bash
# Read payload from stdin
PAYLOAD=$(cat)
TOOL_INPUT=$(echo "$PAYLOAD" | jq '.tool_input')

# Inject AWS credentials into the command
NEW_COMMAND="AWS_ACCESS_KEY_ID=xxx AWS_SECRET_ACCESS_KEY=yyy $(echo "$TOOL_INPUT" | jq -r '.command')"
NEW_INPUT=$(echo "$TOOL_INPUT" | jq --arg cmd "$NEW_COMMAND" '.command = $cmd')

# Return the mutated input
echo "{\"hookSpecificOutput\": {\"toolInput\": $NEW_INPUT}}"
```

### Hook Process Environment: `$HARNX_PACKAGE_DIR`

Every hook command (both `claude-command` and `claude-command-persistent`) runs
through a shell, and Harnx injects one extra environment variable:

| Variable | Value |
|---|---|
| `HARNX_PACKAGE_DIR` | The directory of the package that owns the hook or tool server (`<config>/packages/<name>/`). For hooks or tool servers defined outside a package (the global `config.yaml`, or servers under `<config>/mcp_servers/` or `<config>/tool_servers/`), it falls back to the config directory (`~/.config/harnx` by default). |

This lets a package bundle helper scripts alongside its config and reference
them without hardcoding an absolute path. For example, a packaged MCP server can
ship `packages/<name>/hooks/jira-auth-hook.py` and invoke it from a hook:

```yaml
    command: >-
      harnx-proxy-auth --hook $HARNX_PACKAGE_DIR/hooks/jira-auth-hook.py
```

Because the command runs through a shell, `$HARNX_PACKAGE_DIR` (along with `~`
and other `$VARS`) is expanded before the hook binary sees it. Note this
expansion applies to the hook `command` only — an MCP server's own `command`
and `args` are spawned directly and are **not** shell-expanded.

This variable is also ambiently exported by the NATS tool-server supervisor to
all spawned tool-server processes (and inherited by child process chains), allowing
bundled hooks to resolve `$HARNX_PACKAGE_DIR/...` paths when tool servers run
bridged over NATS (e.g. `bash` wrapped in `harnx-mcp-hooks-proxy`).

## 7. Permissions

The `permissionDecision` field controls whether a tool is allowed to run.

*   **`allow`**: Execution continues.
*   **`deny`**: Execution is blocked. The agent receives a message indicating the tool was blocked.
*   **`ask`**: Harnx prompts the user to allow or deny the execution. The `permissionDecisionReason` is shown to the user.

**Shorthand**: A hook script can exit with code `2` to immediately signal a `deny` decision without printing a JSON response.

## 8. Resume

The `resume` field is primarily used with the `Stop` event. It allows a hook to decide if the agent should continue thinking or performing tasks.

*   **Loop Protection**: To prevent infinite loops, the `max_resume` setting (default: 3) limits how many consecutive resumes can be triggered.
*   **Async Drain**: Harnx waits for all pending async hooks to finish before checking if a resume was requested. This ensures that any context gathered by background tasks is available to the agent in the next turn.

## 9. Persistent Hooks

Persistent hooks (`type: claude-command-persistent`) use a JSONL-based protocol to avoid the overhead of spawning a new process for every event.

### Request Format (Harnx to Hook)
```json
{"id": "1", "session_id": "...", "cwd": "...", "hook_event_name": "PreToolUse", ...}
```

### Response Format (Hook to Harnx)
```json
{"id": "1", "additionalContext": "...", "hookSpecificOutput": {...}}
```

The hook process must read one line from `stdin`, process it, and write exactly one line to `stdout`. The `id` must match the request.


### Startup Message for Proxy-style Persistent Hooks

Some persistent hooks use an extended JSONL handshake before normal request/response traffic begins. After the hook prints `READY`, the caller may send one startup message shaped like:

```json
{"id": "startup-1", "event": "startup", "vars": {"temp_file_root": "/tmp/harnx-123", "proxy_port": 8443}}
```

`vars` uses same safe request-vars block later per-request messages can carry. For `harnx-proxy-auth`, this includes values such as:

- `temp_file_root`: per-run writable temp directory the hook can populate before sandboxed command starts
- `proxy_port`: local proxy port, available because startup message is sent after proxy listener is ready

Hook should respond with normal single-line JSON object using same `id`. It may include an `env` object:

```json
{"id": "startup-1", "env": {"ACLI_CONFIG_DIR": "/tmp/harnx-123/acli"}}
```

Only string values from `env` are kept. Caller merges them into sandboxed command environment with this precedence:

1. `tool_input.env`
2. `--env` jaq scripts
3. hook startup `env`
4. proxy defaults such as `HTTPS_PROXY` and CA-bundle variables

This lets startup hook fill gaps without overriding explicit tool-call env or jaq-derived env. Startup hook can also write files directly under `temp_file_root` before command starts.

### Backwards Compatibility

Startup message is optional extension. Older hooks that ignore `event: "startup"`, or reply without an `env` field, still work: caller treats missing/unknown startup env as empty and continues with normal per-request traffic. Keeping lazy per-request initialization paths in hook code remains valid defensive fallback.
### Surfacing Notices to the UI

A persistent hook can push a message to the active interface (TUI/CLI/serve) at
any time by writing a **standalone** JSONL line — one that carries **no `id`**,
so it is not treated as a response:

```json
{"notice": {"level": "error", "message": "Atlassian auth unavailable — Jira calls will fail"}}
```

`level` is `error`, `warning`, or `info` (default `warning`). Harnx emits it as
a user-visible `Notice`. This is the recommended way for a live hook to report
an internal problem (it keeps running and answering requests). `harnx-proxy-auth`
forwards `notice` lines emitted by its exec sub-hooks, so a nested hook (e.g.
`jira-auth-hook.py`) can surface errors up the chain.

If a persistent hook process instead **exits unexpectedly** (bad flag, crash),
Harnx automatically emits an Error notice containing the tail of its captured
stderr — so a dead hook is never silent.

## 10. Examples

### 1. AWS Credential Injector (PreToolUse Mutation)

Injects AWS credentials into any `bash_exec` call.

```yaml
# config.yaml
hooks:
  entries:
    - event: PreToolUse
      type: claude-command
      matcher: bash_exec
      command: "inject-aws-creds.sh"
```

**inject-aws-creds.sh:**
```bash
#!/bin/bash
# Replaces command with one prefixed with AWS env vars
input=$(jq -r '.tool_input.command')
new_input=$(jq -n --arg cmd "AWS_REGION=us-east-1 $input" '{"command": $cmd}')
echo "{\"hookSpecificOutput\": {\"toolInput\": $new_input}}"
```

### 2. GitHub Auth Injector (PreToolUse Mutation)

Intercepts GitHub API calls or git commands to inject a token.

```bash
#!/bin/bash
input=$(cat)
tool_name=$(echo "$input" | jq -r '.tool_name')

if [[ "$tool_name" == "git_clone" ]]; then
  url=$(echo "$input" | jq -r '.tool_input.url')
  # Rewrite https://github.com/... to https://<token>@github.com/...
  new_url=${url/https:\/\/github.com/https://$GH_TOKEN@github.com}
  echo "{\"hookSpecificOutput\": {\"toolInput\": {\"url\": \"$new_url\"}}}"
else
  echo "{}"
fi
```

### 3. Tool Deny-list (Block Tools)

Prevents the use of `bash_exec` for security reasons.

```yaml
hooks:
  entries:
    - event: PreToolUse
      type: claude-command
      matcher: bash_exec
      command: "exit 2"
      status_message: "Blocking bash_exec for safety"
```

### 4. Manual Tool Confirmation (Ask)

Requires user approval before any tool runs. See the [Tool Confirmation Guide](tool-confirmation-guide.md) for a full walkthrough.

```yaml
hooks:
  entries:
    - event: PreToolUse
      type: claude-command
      command: >-
        printf '%s\n' '{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"Manual approval required"}}'
```

When the agent calls a tool, Harnx pauses and shows a confirmation prompt:

```text
Hook requires confirmation for tool 'bash_exec'
Reason: Manual approval required
Input: {
  "command": "ls -la"
}
Allow this tool call? (y/N)
```

The default is **No** (deny). Use `matcher` to limit confirmation to specific tools:

```yaml
hooks:
  entries:
    - event: PreToolUse
      type: claude-command
      matcher: "bash_exec|bash_spawn"
      command: "/path/to/ask-confirm.sh"
```

### 5. Audit Logger (Async PostToolUse)

Logs all tool results to a file in the background.

```yaml
hooks:
  entries:
    - event: PostToolUse
      type: claude-command
      command: "tee -a tool_audit.log"
      async: true
```

## 11. Auth Proxy (`harnx-proxy-auth`)

The `harnx-proxy-auth` binary is a specialized persistent hook that solves the problem of injecting authentication headers into HTTPS requests made by tools like `curl`, `git`, or custom scripts, without having to manually rewrite every command.

### Feature Overview

It acts as a local HTTPS MITM (Man-in-the-Middle) proxy. When configured as a `claude-command-persistent` hook for `bash_exec` or `bash_spawn`, it:
1. Starts a local proxy server.
2. Generates an ephemeral CA certificate.
3. Injects proxy configuration environment variables into the tool's environment.
4. Transparently injects headers (like `Authorization`) into requests that match configured URL patterns.

### Installation

`harnx-proxy-auth` is built alongside the main `harnx` binary. You can install it via `cargo install --path crates/harnx-proxy-auth` from the repository root to ensure it's in your `PATH`.

### CLI Flags

Primary configuration is done via `--hook`:

`--hook <JQ_FILTER>`

Each hook is jq/jaq filter expression applied to JSON request object with fields:

```json
{
  "method": "GET",
  "host": "github.com",
  "path": "/repos/foo/bar",
  "headers": {
    "authorization": "existing-value",
    "content-type": "application/json"
  }
}
```

Filter should return the same object, optionally with:

- **Modified `headers`** — patch the request headers (null removes a header, string upserts it).
- **`block` field** — set `.block = true` or `.block = "reason"` to reject the request with a `403 Forbidden` response instead of forwarding it. Use this to prevent requests to specific hosts or paths.

Multiple `--hook` flags are combined as a pipe: `hook1 | hook2 | ...`.

#### Executable (script) hooks and the `vars` block

A `--hook` argument that is a path to an executable (or an inline shebang
script) runs as a resident process speaking the JSONL protocol: it reads one
request object per line from `stdin` (each carrying a correlation `id`) and
writes one response line per `id`. In addition to `method`/`host`/`path`/
`headers`, each request includes a **`vars`** object with the resolved,
non-secret context that jq hooks reference as jaq variables:

```json
{
  "id": "evt-42",
  "vars": {
    "fake_uuid_key": "…", "fake_base64_key": "…", "fake_url_base64_key": "…",
    "fake_hex_key": "…", "fake_email": "…",
    "temp_file_root": "/tmp/harnx-fs-XXXXXX"
  },
  "method": "GET", "host": "api.example.com", "path": "/", "headers": {}
}
```

`temp_file_root` is populated only when `--fs` is passed (proxy-auth's
per-instance temp dir). Real secrets are **not** placed in `vars` — an
executable hook already inherits proxy-auth's process environment, so it reads
real tokens directly from its own `os.environ` (or equivalent). Keeping secrets
out of the request payload avoids duplicating them into a surface that could be
logged. Use `vars.temp_file_root` when a hook needs to write files into the same
directory that a sibling `--env`/`--fs` exposes to the sandbox.

Examples:

```sh
# Single host — exact match only
harnx-proxy-auth --hook 'if .host == "github.com" then .headers.authorization = "Bearer \(env.GITHUB_TOKEN)" end'

# GitHub hosts allowlist — use explicit equality, not endswith() which would match naughtygithub.com
harnx-proxy-auth --hook 'if (.host == "github.com" or .host == "api.github.com" or .host == "uploads.github.com" or .host == "objects.githubusercontent.com")
  then .headers.authorization = "Bearer \(env.GITHUB_TOKEN // env.GH_TOKEN)"
  end'

# Block requests to a specific host (returns 403 to the client)
harnx-proxy-auth --hook 'if .host == "blocked.example.com" then .block = "host not allowed" end'

# Block and inject auth in one filter
harnx-proxy-auth --hook '
  if .host == "blocked.example.com" then .block = true
  elif .host == "api.github.com" then .headers.authorization = "Bearer \(env.GITHUB_TOKEN)"
  else . end'
```

### Hook Configuration

Add it to your `config.yaml` as persistent hook for bash tools:

```yaml
hooks:
  entries:
  - event: PreToolUse
    type: claude-command-persistent
    matcher: "bash_exec|bash_spawn"
    command: >-
      harnx-proxy-auth
      --hook 'if (.host == "github.com" or .host == "api.github.com" or .host == "uploads.github.com" or .host == "objects.githubusercontent.com")
          then .headers.authorization = "Bearer \(env.GITHUB_TOKEN // env.GH_TOKEN)"
          end'
```

### Sentinel Environment Variable Injection (`--env`)

The `--env '<jaq-script>'` flag allows you to generate fake "sentinel" credentials at startup and inject them into bash tool calls. Hook scripts can then match on those sentinel values and replace them with real credentials — keeping real tokens out of tool call arguments entirely.

#### Purpose

When an agent uses a tool like `curl` or `git`, any credentials passed as arguments (e.g., `-H "Authorization: Bearer $TOKEN"`) are visible in the process list and logs. By using sentinels, you can provide the tool with a unique, session-specific "fake" token. The `harnx-proxy-auth` hook recognizes this fake token as it passes through the proxy and swaps it for the real one just before the request leaves the machine.

#### Execution Model

1. **Startup**: The `--env` script receives `{}` as input and must output a JSON object where keys are environment variable names and values are their sentinel values. **All values must be strings** — non-string values (numbers, booleans, objects) are rejected at startup.
2. **Order**: Multiple `--env` flags run in order; later keys overwrite earlier ones.
3. **Validation**: Any compilation, runtime, or type error in an `--env` script will abort startup.
4. **Precedence**: Injection follows this order (highest priority wins):
    - `tool_input.env` (user-provided keys always win)
    - **Sentinel environment variables** (from `--env` flags)
    - Proxy variables (`HTTP_PROXY`, etc.)

#### Available Sentinel Variables

The following jaq variables are available to each `--env` script to help generate unique keys:

| Variable | Description | Example |
| :--- | :--- | :--- |
| `$fake_uuid_key` | UUID string with dashes | `550e8400-e29b-41d4-a716-446655440000` |
| `$fake_base64_key` | Standard Base64 of the UUID bytes | `VQ6EANKbQdSnbEVmVESAAA==` |
| `$fake_url_base64_key` | URL-safe no-pad Base64 of the UUID bytes | `VQ6EANKbQdSnbEVmVESAAA` |
| `$fake_hex_key` | Lowercase hex (32 characters) | `550e8400e29b41d4a716446655440000` |
| `$fake_email` | UUID with first `-` replaced by `@` | `550e8400@e29b-41d4-a716-446655440000` |

#### Helper Functions

Two helper functions are provided for formatting authentication headers:

- **`bearer(token)`**: Returns `"Bearer <token>"`.
- **`basic(user; pass)`**: Returns `"Basic <base64(user:pass)>"`.

#### Worked Examples

##### GitHub Token Injection

The `--env` script overrides `GITHUB_TOKEN` in the bash tool's environment with a session-unique sentinel value. The tool sends that sentinel in the `Authorization` header. The `--hook` script sees the sentinel value in the outbound request and swaps in the real `GITHUB_TOKEN` (still in the proxy's own environment) before the request leaves the machine.

```yaml
hooks:
  entries:
  - event: PreToolUse
    type: claude-command-persistent
    matcher: "bash_exec|bash_spawn"
    command: >-
      harnx-proxy-auth
      --env 'if (env.GITHUB_TOKEN // env.GH_TOKEN) then .GITHUB_TOKEN = "ghs_\($fake_base64_key)" else . end'
      --hook 'if (.host == "api.github.com") and (.headers.authorization == "Bearer ghs_\($fake_base64_key)")
          then .headers.authorization = bearer(env.GITHUB_TOKEN // env.GH_TOKEN)
          else . end'
```

### Atlassian CLI (`acli`) Example

`acli` (the Atlassian CLI for Jira, Confluence, etc.) stores API tokens in the OS keyring, which is inaccessible inside the sandboxed environment. `harnx-proxy-auth` solves this by automatically sourcing the token from your host OS keyring at startup. Once you have logged in with an API token on the host, no further manual environment variables or `.env` entries are required.

> **API-token auth only — OAuth is not supported.** This flow replays your stored credential as an HTTP Basic auth password, so `acli` must be authenticated with an **API token** (Step 1 below). It does **not** work if you logged in with OAuth (`acli jira auth login --web`): OAuth stores a short-lived, rotating bearer token as a compressed binary blob the proxy can neither read (it is not valid UTF-8, so it degrades to `null` and no synthetic config is injected) nor replay as Basic auth — sandboxed `acli` then fails with `unauthorized: use 'acli jira auth login' to authenticate`. If `acli jira auth status` reports `Authentication Type: oauth`, run `acli jira auth logout` and repeat Step 1 with an API token.

#### Step 1 — Log in with your real token (once, on the host)

Run `acli jira auth login` **outside of a harnx session** (on the host) with your real Atlassian API token. This is the only time the real token is used — after this, the proxy takes over inside harnx:

```sh
echo "<your-real-api-token>" | acli jira auth login \
  --site <your-site>.atlassian.net \
  --email <your-email@example.com> \
  --token
```

This writes profile metadata (site, email, cloud ID) to `~/.config/acli/jira_config.yaml` and stores the token in the OS keyring. Inside the harnx birdcage, `acli` will attempt requests using the sentinel token from its synthetic config. The proxy intercepts these requests and replaces the sentinel with the real token sourced from your host keyring.

The same command applies to other `acli` products: replace `jira` with `confluence`, `assets`, etc.

> **Synthetic Config:** `acli` inside the sandbox uses a synthetic `jira_config.yaml` generated by the proxy in a private temporary directory. It does **not** require access to your host `~/.config/acli/` directory, which can remain isolated from the sandbox.

> **`acli jira auth status` reports "Unauthorized" inside the sandbox — this is expected.** That command performs a **local** check: it decrypts and validates the token stored in the config, and never makes a network request. Inside the sandbox the stored token is the *sentinel* (the real token deliberately never enters the sandbox), so the local validation fails. Actual Jira operations — `acli jira workitem view`, `project list`, etc. — still work, because those make network calls that the proxy authenticates on the wire by swapping the sentinel for the real token. Judge success by whether data commands work, not by `auth status`.

#### Step 2 — Configure the proxy hook

Add the following to your `config.yaml`. The bundled `jira-auth-hook.py` does all the work: it reads your host `acli` profile, sources the real token from the OS keyring, writes a synthetic `jira_config.yaml` — holding only a sentinel token, written as a YAML `!!binary` scalar so `acli` accepts it — into the proxy's per-run temp dir, and replaces the sentinel with the real token on outbound `api.atlassian.com` / site requests. The `--fs`/`--env` lines allocate that temp dir and point `ACLI_CONFIG_DIR` at it.

The hook ships with the `harnx` **pantheon** and **coding** packages at `~/.config/harnx/packages/<pkg>/hooks/jira-auth-hook.py` (swap `pantheon` below for `coding` if that's the package you use). For a standalone `harnx-sandbox-run` install, see the download-and-verify step in [sandbox-run.md](./sandbox-run.md).

No manual environment variables are required as long as you have logged in with an **API token** (Step 1) on your host — OAuth (`--web`) is not supported.

```yaml
hooks:
  entries:
  - event: PreToolUse
    type: claude-command-persistent
    matcher: "bash_exec|bash_spawn"
    command: >-
      harnx-proxy-auth
      --fs '{"harnx-fs-acli/acli/.keep": ""}'
      --env '{"ACLI_CONFIG_DIR": "\($temp_file_root)/harnx-fs-acli"}'
      --hook ~/.config/harnx/packages/pantheon/hooks/jira-auth-hook.py
```

The hook sources the token from the platform keyring automatically (`secret-tool` on Linux, `security find-generic-password` on macOS); set `HARNX_JIRA_TOKEN_CMD` to use a different secret store.

> **Security note:** The hook injects the token only for the site in your active `acli` profile (plus `api.atlassian.com`), preventing credentials from being accidentally forwarded to other Atlassian tenants.

You can combine Atlassian and GitHub auth in a single `harnx-proxy-auth` invocation:

```yaml
hooks:
  entries:
  - event: PreToolUse
    type: claude-command-persistent
    matcher: "bash_exec|bash_spawn"
    command: >-
      harnx-proxy-auth
      --env 'if (env.GITHUB_TOKEN // env.GH_TOKEN) then .GITHUB_TOKEN = "ghs_\($fake_base64_key)" else . end'
      --hook 'if (.host == "api.github.com") and (.headers.authorization == "Bearer ghs_\($fake_base64_key)")
          then .headers.authorization = bearer(env.GITHUB_TOKEN // env.GH_TOKEN)
          else . end'
      --fs '{"harnx-fs-acli/acli/.keep": ""}'
      --env '{"ACLI_CONFIG_DIR": "\($temp_file_root)/harnx-fs-acli"}'
      --hook ~/.config/harnx/packages/pantheon/hooks/jira-auth-hook.py
```

### Injected Environment Variables

When the hook runs, it injects the following variables into the tool's environment:

| Variable | Purpose |
| :--- | :--- |
| `HTTPS_PROXY` | Directs HTTPS traffic to the local proxy. |
| `SSL_CERT_FILE` | Points to the ephemeral CA certificate so tools trust the proxy. |
| `REQUESTS_CA_BUNDLE` | Used by Python `requests` library. |
| `CURL_CA_BUNDLE` | Used by `curl`. |
| `NODE_EXTRA_CA_CERTS` | Used by Node.js. |
| `GIT_SSL_CAINFO` | Used by `git`. |

### Security & Precedence

*   **User Precedence**: If the agent or user already defined any of these environment variables in the tool call, `harnx-proxy-auth` will **not** overwrite them. This allows for manual overrides.
*   **Ephemeral CA**: The proxy CA certificate is generated on startup and deleted immediately upon the hook's exit. It is only trusted by the processes spawned during a single harnx run.
*   **Scoped Injection**: Request mutation scope is defined by your jaq filter. Requests that do not match your condition should return `.`, leaving traffic unchanged.
