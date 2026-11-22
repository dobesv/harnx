# Hooks Guide

Lifecycle event hooks run short Bash or Python scripts inside the harnx worker
process when a session starts, a prompt is submitted, or a turn ends. They can
inject context, reject or rewrite a prompt, and resume a stopped turn — without
spawning or supervising any external hook process.

For scripts that wrap tool calls (the old `PreToolUse`/`PostToolUse` use
cases: deny, ask, mutate arguments and results), use
[tool middleware](middleware-guide.md). Existing Claude-protocol hook commands
can run through the [Claude hooks compatibility tool](middleware-claude-hooks.md).

## Events and operations

| Event | When it fires | Operations | Effect |
| :--- | :--- | :--- | :--- |
| `SessionStart` | Once per session, before the first model call. Resuming a session does not fire it again. | `add_context` | Context reaches the first model call. |
| `UserPromptSubmit` | When the user submits a prompt, before it is persisted or sent to the model. | `add_context`, `block`, `rewrite_prompt` | Blocking rejects the turn; a rewrite replaces the prompt the model sees. |
| `Stop` | When the agent finishes its turn normally. | `resume` | The turn continues with the given message, bounded by `max_resume`. |
| `StopFailure` | When a turn fails. Observe-only. | — | Diagnostics are available through `event_input`. |

Every hook can also call `event_input` (read the event payload) and `fetch`
(HTTP(S) requests to the entry's allowlisted hosts). Interaction operations
(`confirm`, `save`, `next`, …) are deliberately not registered: an event hook
that calls one fails, and the entry's `on_error` policy decides what happens.

## Configuration

`hooks:` is a map from event name to a list of script entries. The same
`hooks:` block can appear at three levels:

1. **Global** (`config.yaml`): applies to every session and agent.
2. **Tool server / package** (`tool_servers/*.yaml`, package manifests): hooks
   declared by a tool server or package.
3. **Agent** (agent `.md` front-matter): applies to sessions bound to that agent.

Entries resolve per event in the order global → tool server/package → agent:

* A later entry with the same `name` replaces the earlier one in place.
* An entry `{name: ..., enabled: false}` removes the earlier entry with that
  name (it must exist — disabling an unknown hook is a config error).
* Names must be unique within one configuration file; the same name in a
  later tool-server file follows the replacement rule above, so packages
  shipping the same hook resolve to a single chain entry per event.

`max_resume` (top-level `hooks:` setting, default 3) bounds how many times
`Stop` hooks may resume a turn. An agent's value overrides the global one.

### Entry fields

| Field | Type | Description |
| :--- | :--- | :--- |
| `name` | string | Required. Unique within a level. |
| `bash` | string | Inline Bash script (mutually exclusive with `python`/`script`). |
| `python` | string | Inline Python script. |
| `script` | path | Script file (`.sh`/`.bash` → Bash, `.py` → Python), resolved relative to the declaring file's directory. |
| `tools` | list | Tool selectors the script may call. Empty (default) exposes no tools. |
| `timeout` | duration | Wall-clock limit per run (default `30s`). |
| `on_error` | `allow`/`deny` | Default `allow`. `deny` fails the event when the script run fails; valid only for `SessionStart` and `UserPromptSubmit` — elsewhere it is a config error. |
| `fetch` | list | Exact HTTP(S) hosts the script may request. Empty (default) denies all. |
| `enabled` | boolean | `false` marks a disable entry. |

Parsing is strict: unknown fields, unknown event names (e.g. `PreToolUse`), and
legacy `command:` entries all fail config load with a migration message.

### Example

```yaml
# config.yaml
hooks:
  max_resume: 1
  SessionStart:
    - name: session-context
      tools: [fs_read]
      bash: add_context "$(fs_read --path RUNBOOK.md -o text)"
  UserPromptSubmit:
    - name: no-secrets
      bash: |
        payload=$(event_input)
        case "$payload" in *password*) block "secrets are not allowed in prompts";; esac
      on_error: deny
  Stop:
    - name: keep-going
      script: hooks/keep-going.sh
  StopFailure:
    - name: audit
      python: |
        payload = event_input()
        print(payload.get("error", "unknown"))
```

An agent can override one event and disable another without touching the rest:

```yaml
# agents/example-agent.md front-matter
---
hooks:
  Stop:
    - name: keep-going   # replaces the global entry with this name
      bash: resume "finish the checklist first"
  SessionStart:
    - name: session-context
      enabled: false      # removes the global entry for this agent
---
```

## Writing hook scripts

Hooks run in the same in-process BashKit/Monty script runtime as tool
middleware. Each event's operations are registered as Bash commands and Python
functions.

**Bash** — operations are commands:

```bash
add_context "remember this"
block "not allowed"
rewrite_prompt "cleaned-up prompt"
resume "keep going"
payload=$(event_input)   # JSON string
fetch https://example.com/info
```

**Python** — operations are functions:

```python
payload = event_input()          # dict
add_context("remember this")
block("not allowed")
rewrite_prompt("cleaned-up prompt")
resume("keep going")
fetch("https://example.com/info")
```

The `event_input` payload contains `event`, `session_id`, `cwd`,
`resume_count`, and the event-specific fields:

| Event | Extra fields |
| :--- | :--- |
| `SessionStart` | `source`, `model` |
| `UserPromptSubmit` | `prompt` (the prompt as this hook sees it — rewritten if an earlier hook rewrote it) |
| `Stop` | `last_assistant_message` |
| `StopFailure` | `error`, `error_type` |

`resume` may be called with an empty message. Effects from a failed run are
discarded entirely.

## Calling tools

Hook entries can declare an opt-in `tools:` allowlist to call tools from Bash
or Python scripts. Selectors use the same syntax and resolution as agent
`use_tools` and middleware `tools:` (tool names, wildcards like `fs_*`, or
package-qualified names). An omitted or empty `tools:` list gives the script
no tools.

### Bash and Python examples

In Bash, each allowlisted tool is available as a builtin command with standard
CLI flags (`--kebab-case`, `--json`, `-o json|text`).

`add_context` takes its message as an argument rather than reading standard input,
so pass tool output using command substitution:

```bash
# Entry declares tools: [plans_get_plan]
plan_text=$(plans_get_plan --plan "cid:plan:team/main/sprint" -o text)
add_context "$plan_text"

# Or in a single statement:
add_context "$(plans_get_plan --plan "cid:plan:team/main/sprint" -o text)"
```

In Python, call tools with `call_tool(name, args)`:

```python
# Entry declares tools: [fs_read]
result = call_tool("fs_read", {"path": "RUNBOOK.md"})
add_context(result["content"][0]["text"])
```

If an entry omits `tools:`, no tool commands exist in Bash (calling one returns
command not found), and `call_tool` is not registered in Python.

### Allowlist and exclusions

Tools are strictly filtered before execution:

* **Host-side allowlist:** Only tools matching the entry's resolved allowlist
  can be dispatched. In Bash, unlisted tools are not registered; invoking one
  returns exit 127 (unknown command), which stops the script under the default
  `set -e`. In Python, calling `call_tool` on an unlisted tool stops the
  interpreter immediately. Neither language ever dispatches an unlisted tool.
* **Exclusions:** Interactive and agent-steering tools are never exposed to hook
  scripts, even when a selector like `*` matches them. Specifically, `bash`,
  `ask_user`, `switch_agent`, and every `*_session_handoff` tool are excluded.

### Recoverable errors vs host failures

Tool execution outcomes distinguish recoverable errors from host failures:

* **Recoverable errors return to the script:** When a tool returns a provider
  error, is blocked by tool middleware, or requests user interaction
  (`interactions are not supported here`), the tool call returns an error-flagged
  result to the script (`is_error: true` in JSON, or `result["is_error"]` and
  `result["error"]` in Python). The script keeps running, can inspect the
  result, and effects recorded afterward (such as `add_context`) still apply.
  In Bash, recoverable tool errors exit 0 and write `{"is_error": true, ...}` to
  stdout, so direct command substitution like `add_context "$(tool ...)"` injects
  the error JSON into context. Inspect the output with `jq` first:

  ```bash
  raw=$(plans_get_plan --plan "cid:plan:team/main/sprint" -o json)
  if [ "$(echo "$raw" | jq -r '.is_error // false')" = "false" ]; then
    add_context "$(echo "$raw" | jq -r '.content[0].text')"
  fi
  ```
* **Host-level failures stop the script:** In Python, calling an unlisted tool
  stops the script immediately. Fatal evaluation errors and script cancellation
  (hook timeout or session abort) stop execution immediately in both languages.
  In Bash, an unlisted tool is an unknown command (exit 127), so it stops the
  script unless handled or errexit is disabled. When a run fails, all effects
  recorded during that run are discarded, and the entry's `on_error` policy
  applies (`allow` drops effects and continues to the next hook; `deny` fails
  the event).

### Interactions not supported

Hook scripts run autonomously and cannot suspend for user input. If a tool or
its middleware attempts an interaction (`confirm`, `save`, `answer`, `next`),
the call fails recoverably with an error indicating that interactions are not
supported.

### Tool middleware applies

Hook-originated tool calls pass through configured tool middleware chains.
Policy middleware, audit logging, and credential injectors (such as `proxy_auth`
or `aws_creds`) remain effective on tool calls made by hooks.

### Transcript visibility

Tool calls made by lifecycle hooks are internal helper calls. They do not
append `ToolCalls` or `ToolResults` entries to the session transcript, and they
do not produce UI tool-call events.

### Operation name collisions

Tool names must not collide with registered event-hook operations for that event
(`event_input`, `fetch`, `add_context`, `block`, `rewrite_prompt`, `resume`).
If an allowlisted tool shares a name with an operation on that event, the hook
entry is rejected before running (`allowlisted tool collides with an event-hook operation`).
Neither command runs, effects are dropped, and `on_error` applies.

### Helper tool server startup on workers

When the NATS worker daemon manages tool servers locally, servers required only
by hook `tools:` selectors start automatically on session activation. You do not
need to list hook tools in the agent's `use_tools` to start their servers.

Selectors are gathered across enabled hook entries for all four events
(`SessionStart`, `UserPromptSubmit`, `Stop`, `StopFailure`). Disabled entries
contribute nothing. If hook configuration fails to resolve, the worker logs a
warning and continues starting servers for agent and middleware selectors.

### Downgrades and rollback

Configuration parsing rejects unknown fields (`deny_unknown_fields`). If you
roll back to an older harnx version that lacks hook tool support, any hook entry
declaring `tools:` will fail config load. Remove `tools:` from hook entries
before downgrading.

## Composition

Hooks run sequentially in declaration order, per event:

* `add_context` texts accumulate in order.
* A later `rewrite_prompt` replaces an earlier one; following hooks see the
  rewritten prompt through `event_input`.
* The first `block` stops the chain, skips later hooks, and discards all
  accumulated context for the event.
* The first `resume` wins; remaining `Stop` hooks are skipped.
* With `on_error: allow` a failed run's effects are dropped and later hooks
  still run. With `on_error: deny` the event fails with an error naming the
  hook.

## Where added context goes

Context from `SessionStart`/`UserPromptSubmit` hooks is appended to the
conversation as its own durable user row and reaches the next model call —
including mid-turn rounds and NATS HA worker turns, where the prompt itself is
folded out of the durable log (issue #2198).

The row carries a reserved `hook-context-v1-<uuid>` message id, so harnx never
treats it as an executable prompt or pending input. Binaries from before this
prefix existed classify such rows as ordinary user prompts. In a mixed-version
cluster sharing session logs, upgrade every session-log reader before enabling
event hooks that call `add_context` — the same reader-first ordering as the
[middleware rollout](middleware-guide.md#cluster-rollout-and-rollback).

`rewrite_prompt` replaces the prompt in the persisted user row on a local
turn. On a NATS worker turn the append-only durable log keeps the original
text; the rewrite is applied to the folded rows in memory and re-derived if the
hook chain runs again (`rewrite_durable_prompt_rows` in
`crates/harnx-runtime/src/agent_loop.rs`).

## Legacy process hooks

`hooks:` previously configured external hook-server processes (`command:`
entries, `entries:` lists, `PreToolUse`/`PostToolUse`/`InstructionsLoaded`/`
`CwdChanged` events) supervised over NATS. That configuration model is gone:
a legacy entry now fails config load with a message pointing at its
replacement. Migration:

* **Tool gating / mutation** (`PreToolUse`, `PostToolUse`): use
  [tool middleware](middleware-guide.md). Claude-protocol commands run through
  the [Claude compatibility adapter](middleware-claude-hooks.md).
* **Lifecycle events** (`SessionStart`, `UserPromptSubmit`, `Stop`,
  `StopFailure`): use the in-process entries described above.
* **`harnx-sandbox-run`** uses `--hook environment <COMMAND> [ARGS...] \;`
  to query native credential/proxy environment tools over MCP stdio. Legacy
  JSONL adapters were removed; helper services stay alive until the command exits.

## Auth proxy (`harnx-proxy-auth`) and middleware migration

The `harnx-proxy-auth` binary is a TLS-intercepting authentication proxy that solves the problem of injecting authentication headers into HTTPS requests made by tools like `curl`, `git`, `gh`, `acli`, or custom scripts, without having to manually rewrite commands or expose long-lived secrets to the agent environment.

### Migration Overview: Tool Server & Middleware Architecture

Previously, `harnx-proxy-auth` ran as a native hook attached directly to `bash.yaml`. In modern Harnx configurations (including the bundled `pantheon` and `coding` packages), **`harnx-proxy-auth` runs as an ordinary long-lived tool service (`--tool-server`) paired with in-process tool middleware**:

1. **Proxy Tool Service (`--tool-server`)**:
   `harnx-proxy-auth` runs with `--tool-server`, exposing the `proxy_auth` toolset with the no-argument `environment` tool (invoked as `proxy_auth_environment`).
   When called, `proxy_auth_environment` returns:
   ```json
   {
     "env": {
       "HTTP_PROXY": "http://127.0.0.1:<port>",
       "HTTPS_PROXY": "http://127.0.0.1:<port>",
       "SSL_CERT_FILE": "<ca_cert_path>",
       "NODE_EXTRA_CA_CERTS": "<ca_cert_path>",
       "CURL_CA_BUNDLE": "<ca_cert_path>",
       "REQUESTS_CA_BUNDLE": "<ca_cert_path>",
       "GIT_SSL_CAINFO": "<ca_cert_path>",
       "ACLI_CONFIG_DIR": "<temp_file_root>/harnx-fs-acli"
     },
     "filesystem": {
       "ca_cert_path": "<ca_cert_path>",
       "temp_file_root": "<temp_file_root>"
     }
   }
   ```
2. **Dedicated Tool Server Config (`tool_servers/proxy_auth.yaml`)**:
   `packages/{pantheon,coding}/tool_servers/proxy_auth.yaml` owns all proxy flags (GitHub jaq filters, `--fs` provisioning, `--env`, and resident Jira hook).
3. **Bash Tool Server with In-Process Middleware (`tool_servers/bash.yaml`)**:
   `packages/{pantheon,coding}/tool_servers/bash.yaml` has **no hooks**. Instead, it defines in-process middleware matching `^bash_(exec|spawn)$` that executes `middleware/proxy-auth.py` with `tools: [proxy_auth_environment]`.
4. **In-Process Middleware Script (`middleware/proxy-auth.py`)**:
   The script queries `proxy_auth_environment`, merges the returned proxy environment defaults on top of existing arguments (ensuring caller-provided environment keys always win), and invokes `call_next`.
5. **Supervisor & Namespace Co-Location**:
   - The proxy binds to a loopback address (`http://127.0.0.1:<port>`) and generates CA certificates and synthetic config files on the local disk. Therefore, **`harnx-proxy-auth` must share the same network and filesystem namespace as `harnx-bash-tools`**.
   - **Managed Supervisor**: Harnx supervisor automatically co-locates `harnx-proxy-auth` and `harnx-bash-tools` on the bash host.
   - **Unmanaged / Distributed Deployment**: Operators deploy `harnx-proxy-auth --tool-server ...` alongside `harnx-bash-tools` on the bash host in the same network/filesystem namespace, connecting both to a common NATS cluster. The worker executing agent turns and in-process middleware can be remote: middleware calls `proxy_auth_environment` over NATS, merges the environment, and forwards the call to `bash_exec`/`bash_spawn`.
   > **Deployment Warning on Routing Scopes**: Connecting multiple bash hosts or proxy instances to a common NATS URL alone is not sufficient. Because proxies allocate loopback listeners and local filesystem paths (`ca_cert_path`, `temp_file_root`), each bash host and proxy service pair must use distinct routing scopes (`HARNX_SERVER_SCOPE`) and package/config identities to prevent workers from cross-routing requests to a different host.
6. **Standalone Sandbox Execution (`harnx-sandbox-run`)**:
   `harnx-sandbox-run` uses `--hook environment <COMMAND> [ARGS...] \;` to query `proxy_auth_environment` over MCP stdio. Legacy JSONL adapters were removed; `harnx-sandbox-run` manages the sidecar process and keeps it alive until the sandboxed command exits.

### Feature Overview

The proxy acts as a local HTTPS MITM (Man-in-the-Middle) proxy:
1. Starts a local proxy server binding to loopback (`127.0.0.1:<port>`).
2. Generates an ephemeral CA certificate trusted only during the run.
3. Provisions synthetic tool configuration files in a private temporary directory (`temp_file_root`).
4. Serves the `proxy_auth_environment` tool so middleware can inject proxy configuration into bash commands.
5. Transparently injects headers (such as `Authorization`) or returns synthetic responses for requests that match configured URL patterns or resident hooks.

### Installation

`harnx-proxy-auth` is built alongside the main `harnx` binary:

```sh
cargo install --path crates/harnx-proxy-auth
```

### CLI Flags

| Flag | Description |
| :--- | :--- |
| `--tool-server` | Optional legacy compatibility flag; tool server mode is default. Runs as a tool service (`proxy_auth` toolset) over NATS (default), exposing `proxy_auth_environment`. |
| `--mcp-stdio` | Use MCP stdio transport for the tool service. |
| `--mcp-http` | Use MCP Streamable HTTP transport for the tool service. |
| `--host <HOST>` | MCP HTTP bind host (default: `0.0.0.0`; requires `--mcp-http`). |
| `--port <PORT>` | MCP HTTP port (default: `3009`; requires `--mcp-http`). |
| `--enable-tool <GLOB>` | Enable only matching tools in tool-server mode. |
| `--name <NAME>` | Server name assigned by supervisor. |
| `--hook <HOOK>` | A jaq filter, script path (`/`, `./`, `../`), inline script (`#!...`), or package script (`$HARNX_PACKAGE_DIR/...`) applied to each request. Receives `{host, path, method, headers}`. Can be repeated. |
| `--hook-timeout-secs <SECS>` | Per-request timeout in seconds for exec `--hook` stages (default: 30). |
| `--env <JQ_FILTER>` | A jaq filter to generate extra environment variables, with access to sentinels, `$temp_file_root`, and `$proxy_port`. |
| `--fs <JQ_FILTER>` | A jaq filter generating relative files under a private temp directory (`$temp_file_root`). |
| `--load-yaml <NAME=PATH>` | Load a YAML file into jaq variable `$NAME` before evaluating hooks. |
| `--load-json <NAME=PATH>` | Load a JSON file into jaq variable `$NAME` before evaluating hooks. |
| `--load-raw <NAME=PATH>` | Load a raw text file into jaq variable `$NAME` before evaluating hooks. |
| `--load-exec <NAME=CMD>` | Execute a command and load stdout into jaq variable `$NAME`. |
| `--log-file <PATH>` | Write a JSON log line for every proxied request. |

#### Executable (script) hooks and the `vars` block

A `--hook` argument that is a path to an executable (or an inline shebang script) runs as a resident process speaking the JSONL protocol: it reads one request object per line from `stdin` (each carrying a correlation `id`) and writes one response line per `id`. In addition to `method`/`host`/`path`/`headers`, each request includes a **`vars`** object with the resolved, non-secret context that jq hooks reference as jaq variables:

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

`temp_file_root` is populated only when `--fs` is passed (proxy-auth's per-instance temp dir). Real secrets are **not** placed in `vars` — an executable hook already inherits proxy-auth's process environment, so it reads real tokens directly from its own `os.environ` (or equivalent). Keeping secrets out of the request payload avoids duplicating them into a surface that could be logged. Use `vars.temp_file_root` when a hook needs to write files into the same directory that a sibling `--env`/`--fs` exposes to the sandbox.

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

### Shipped Production Configuration

In `packages/pantheon` and `packages/coding`:

#### 1. Tool Server: `packages/pantheon/tool_servers/proxy_auth.yaml`
```yaml
command: harnx-proxy-auth
description: Long-lived GitHub/Atlassian auth proxy, used by bash middleware
args:
  - --tool-server
  - --hook
  - 'if .host == "github.com" and (env.GITHUB_TOKEN // env.GH_TOKEN) != null then .headers.authorization = "Basic \(["x-access-token", (env.GITHUB_TOKEN // env.GH_TOKEN)] | join(":") | @base64)" end'
  - --hook
  - 'if (.host == "api.github.com" or .host == "uploads.github.com" or .host == "objects.githubusercontent.com") and (env.GITHUB_TOKEN // env.GH_TOKEN) != null then .headers.authorization = "Bearer \(env.GITHUB_TOKEN // env.GH_TOKEN)" end'
  - --fs
  - '{"harnx-fs-acli/acli/.keep": ""}'
  - --env
  - '{"ACLI_CONFIG_DIR": "\($temp_file_root)/harnx-fs-acli"}'
  - --hook
  - $HARNX_PACKAGE_DIR/hooks/jira-auth-hook.py
```

#### 2. Bash Tool Server: `packages/pantheon/tool_servers/bash.yaml`
```yaml
command: harnx-bash-tools
description: Shell execution — lets agents run commands and scripts

middleware:
  - name: proxy-auth
    match: '^bash_(exec|spawn)$'
    script: middleware/proxy-auth.py
    tools: [proxy_auth_environment]
    timeout: 25h
    on_error: deny

args:
  - --allow-common-default
```

#### 3. Middleware Script: `packages/pantheon/middleware/proxy-auth.py`
```python
# Files are provisioned by the long-lived proxy on the bash host, not in this VFS.
additions = call_tool("proxy_auth_environment", {})
if additions.get("is_error") or additions.get("isError"):
    raise RuntimeError("proxy auth environment lookup failed")

args = tool_input()["arguments"]
caller_env = args.get("env")
if not isinstance(caller_env, dict):
    caller_env = {}
env = additions["env"]
# Existing caller keys win, including explicit proxy/CA overrides.
env.update(caller_env)
args["env"] = env
respond(call_next(args))
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
    - `tool_input.env` (caller-provided keys always win)
    - **Sentinel environment variables** (from `--env` flags)
    - Proxy variables (`HTTP_PROXY`, `HTTPS_PROXY`, etc.)

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
# Inside tool_servers/proxy_auth.yaml:
args:
  - --tool-server
  - --env
  - 'if (env.GITHUB_TOKEN // env.GH_TOKEN) then .GITHUB_TOKEN = "ghs_\($fake_base64_key)" else . end'
  - --hook
  - 'if (.host == "api.github.com") and (.headers.authorization == "Bearer ghs_\($fake_base64_key)")
      then .headers.authorization = bearer(env.GITHUB_TOKEN // env.GH_TOKEN)
      else . end'
```

### Atlassian CLI (`acli`) Setup

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

This writes profile metadata (site, email, cloud ID) to `~/.config/acli/jira_config.yaml` and stores the token in the OS keyring. Inside the sandbox, `acli` will attempt requests using the sentinel token from its synthetic config. The proxy intercepts these requests and replaces the sentinel with the real token sourced from your host keyring.

The same command applies to other `acli` products: replace `jira` with `confluence`, `assets`, etc.

> **Synthetic Config:** `acli` inside the sandbox uses a synthetic `jira_config.yaml` generated by the proxy in a private temporary directory. It does **not** require access to your host `~/.config/acli/` directory, which can remain isolated from the sandbox.

> **`acli jira auth status` reports "Unauthorized" inside the sandbox — this is expected.** That command performs a **local** check: it decrypts and validates the token stored in the config, and never makes a network request. Inside the sandbox the stored token is the *sentinel* (the real token deliberately never enters the sandbox), so the local validation fails. Actual Jira operations — `acli jira workitem view`, `project list`, etc. — still work, because those make network calls that the proxy authenticates on the wire by swapping the sentinel for the real token. Judge success by whether data commands work, not by `auth status`.

#### Step 2 — Configure the proxy tool service

The bundled `jira-auth-hook.py` does all the work: it reads your host `acli` profile, sources the real token from the OS keyring, writes a synthetic `jira_config.yaml` — holding only a sentinel token, written as a YAML `!!binary` scalar so `acli` accepts it — into the proxy's per-run temp dir, and replaces the sentinel with the real token on outbound `api.atlassian.com` / site requests. The `--fs`/`--env` lines allocate that temp dir and point `ACLI_CONFIG_DIR` at it.

The hook ships with the `harnx` **pantheon** and **coding** packages at `$HARNX_PACKAGE_DIR/hooks/jira-auth-hook.py`.

```yaml
# tool_servers/proxy_auth.yaml
command: harnx-proxy-auth
description: Long-lived Atlassian auth proxy tool service
args:
  - --tool-server
  - --fs
  - '{"harnx-fs-acli/acli/.keep": ""}'
  - --env
  - '{"ACLI_CONFIG_DIR": "\($temp_file_root)/harnx-fs-acli"}'
  - --hook
  - $HARNX_PACKAGE_DIR/hooks/jira-auth-hook.py
```

The hook sources the token from the platform keyring automatically (`secret-tool` on Linux, `security find-generic-password` on macOS); set `HARNX_JIRA_TOKEN_CMD` to use a different secret store.

> **Security note:** The hook injects the token only for the site in your active `acli` profile (plus `api.atlassian.com`), preventing credentials from being accidentally forwarded to other Atlassian tenants.

### Injected Environment Variables

When `proxy_auth_environment` is queried, it provides the following environment variables:

| Variable | Purpose |
| :--- | :--- |
| `HTTP_PROXY` / `HTTPS_PROXY` | Directs HTTP/HTTPS traffic to the local proxy (`http://127.0.0.1:<port>`). |
| `SSL_CERT_FILE` | Points to the ephemeral CA certificate so tools trust the proxy. |
| `REQUESTS_CA_BUNDLE` | Used by Python `requests` library. |
| `CURL_CA_BUNDLE` | Used by `curl`. |
| `NODE_EXTRA_CA_CERTS` | Used by Node.js. |
| `GIT_SSL_CAINFO` | Used by `git`. |
| `ACLI_CONFIG_DIR` | (When configured) Points Atlassian CLI to the synthetic config directory. |

### Security & Precedence

*   **User Precedence**: Middleware merges caller environment variables on top of proxy additions (`env.update(caller_env)`). If the agent or caller already defined any of these environment variables in the tool call, the caller values win.
*   **Ephemeral CA**: The proxy CA certificate is generated on startup and deleted upon exit. It is only trusted by processes spawned on the bash host during the session.
*   **Scoped Injection**: Request mutation scope is defined by your jaq filter or resident hook. Requests that do not match return `.`, leaving traffic unchanged.

### Standalone Sandbox Run Usage

For standalone commands, `harnx-sandbox-run` queries the native `environment {}` tool over MCP stdio and keeps the service alive until the command exits:

```bash
harnx-sandbox-run \
  --hook environment harnx-proxy-auth \
    --fs '{"harnx-fs-acli/acli/.keep": ""}' \
    --env '{"ACLI_CONFIG_DIR": "\($temp_file_root)/harnx-fs-acli"}' \
    --hook "$SHIM_DIR/jira-auth-hook.py" \
  \; \
  -- acli jira workitem list
```

## Credential tool services and bash middleware

`harnx-aws-creds --tool-server` and `harnx-k8s-creds --tool-server` are ordinary long-lived NATS/MCP tool servers, not PreToolUse hooks. Their environment tools supply capabilities and file paths before `bash_exec` and `bash_spawn`; actual credentials are resolved on demand by existing authenticated loopback HTTP handlers.

| Service | Tool (input `{}` only) | Additions |
|---------|-----------------------|-----------|
| AWS | `aws_creds_environment` | `env`: `AWS_CONTAINER_CREDENTIALS_FULL_URI`, `AWS_CONTAINER_AUTHORIZATION_TOKEN`, `AWS_REGION`; `filesystem: {}` |
| Kubernetes | `k8s_creds_environment` | `env: {KUBECONFIG}`; `filesystem: {kubeconfig_path}` |

AWS provisions no files. Kubernetes retains a synthetic `NamedTempFile` in the OS temp directory for the service lifetime and removes it on graceful shutdown. Its curl exec plugin calls the local token endpoint using a capability, not a source token or host plugin command.

### Enable the documented configuration

These services are opt-in; neither is enabled in shipped packages. Copy the desired provider YAML and middleware script from [`example_config/credential-tools`](../example_config/credential-tools/README.md) into your active package's `tool_servers/` and `middleware/` directories. Keep your existing `bash.yaml`. Configure at least one host `--context` before enabling Kubernetes; AWS works independently.

Each provider YAML declares middleware matching `^bash_(exec|spawn)$`, allowlists its environment helper and sets `on_error: deny`. The Python stage calls the helper, merges provider additions **over matching caller env keys**, then invokes `next`. Unrelated caller env survives. This preserves the old credential hooks' unconditional overwrite behavior; proxy-auth instead gives caller keys precedence. Missing/null `env` means `{}`; invalid env and helper errors deny before next.

Runtime integration tests load these exact YAML/scripts in managed and independently deployed configurations. Exec/spawn children check env precedence, read the generated kubeconfig and exercise AWS/Kubernetes provider refresh.

### Deployment and security limits

Both credential services need the same loopback network namespace as bash. Kubernetes also needs its generated kubeconfig readable at the same absolute path and `curl` on the child's `PATH`. The supplied local managed services share a host; remote bash/container deployments must arrange shared namespaces/mounts. The worker executing middleware may be remote. Unmanaged services need matching NATS scope/package/config identities; see [AWS setup](aws-creds.md) and [Kubernetes setup](k8s-creds.md) for exact variables.

The tool API returns no raw provider credentials, but authorized AWS clients and Kubernetes curl exec plugins receive actual credentials over HTTP. Host providers control permissions and expiry; static credentials can remain long-lived. Protect endpoint capabilities, synthetic kubeconfig and command output/logs accordingly.

Both binaries support `--mcp-stdio`, `--mcp-http`, `--host`, `--port`, `--name` and `--enable-tool` in tool-server mode. Default MCP HTTP ports are `3010` (AWS) and `3011` (Kubernetes); these listeners are separate from the loopback credential endpoints.

### Standalone sandbox-run usage

Use native MCP stdio environment tools for standalone commands. Credential additions override matching caller keys; proxy additions remain caller-overridable:

```bash
harnx-sandbox-run \
  --hook environment harnx-aws-creds --profile my-profile \; \
  -- aws s3 ls

harnx-sandbox-run \
  --hook environment harnx-k8s-creds --context my-cluster \; \
  -- kubectl get pods
```
