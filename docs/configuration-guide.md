# Configuration Guide

Harnx uses a modular configuration structure. Global settings are defined in a main `config.yaml`, while LLM providers and tool servers are defined in separate YAML files within dedicated subdirectories.

## Configuration Directory

The configuration files are located in `<user-config-dir>/harnx/`. The exact location depends on your operating system:

| OS      | Path                                                    |
| ------- | ------------------------------------------------------- |
| Windows | `C:\Users\Alice\AppData\Roaming\harnx\`               |
| macOS   | `/Users/Alice/Library/Application Support/harnx/`     |
| Linux   | `/home/alice/.config/harnx/`                           |

To find the config directory on your system:

```sh
harnx --info | grep config_file
```

## Folder Structure

Harnx organizes configuration into the following structure:

```text
~/.config/harnx/
├── config.yaml          # Global settings
├── access.yaml          # Agent and session access control rules (optional)
├── users.yaml           # User identity aliases for access checks (optional)
├── clients/             # LLM provider configurations
│   ├── openai.yaml
│   └── claude.yaml
├── tool_servers/        # Tool servers (native toolsets and bridged MCP servers)
│   ├── fs.yaml
│   ├── bash.yaml
│   └── exa.yaml
└── agents/              # Agent definitions (.md files)
    └── coder.md
```

## Main Configuration (`config.yaml`)

The `config.yaml` file contains global behavior and appearance settings.

### LLM

- **model**: Specify the default model to use (e.g., `openai:gpt-4o`).

### Behavior

- **stream**: Whether to use streaming for responses. (`true`/`false`)
- **keybindings**: Choose between `emacs` or `vi` style.
- **editor**: Command used to edit input buffers.
- **wrap**: Text wrapping behavior (`no`, `auto`, or a number).
- **wrap_code**: Whether to wrap code blocks. (`true`/`false`)

### Tool Use

- **tool_use**: Set to `false` to disable all tool use globally.
- **use_tools**: Which tools to enable by default (`*` for all).
- **toolsets**: Group tools into named sets for easy assignment.

## Clients (`clients/`)

Each LLM provider is configured in its own YAML file within the `clients/` directory (e.g., `clients/openai.yaml`).

For a complete list of supported providers and their specific configuration options, see the [**LLM Providers Reference**](providers.md).

**Note:** The **filename** (without `.yaml`) is used as the client's ID in `model` settings (e.g., `openai:gpt-4`). Any `name` field inside the file is ignored.

### General Client Options

```yaml
type: openai              # Provider type (openai, claude, gemini, etc.)
api_key: sk-...           # Optional if <NAME>_API_KEY env var is set
api_base: https://...     # Optional custom endpoint
extra:
  connect_timeout: 10     # seconds to establish TCP/TLS connection (default 10)
  read_timeout: 120       # seconds of read inactivity before stalled response fails (default 120)
patches:                  # Patch API requests using jq expressions
  chat_completions:
    - '.body.cache_control = {"type":"ephemeral"}'
```

`extra.connect_timeout` sets how long Harnx waits to establish TCP/TLS connection before request fails. `extra.read_timeout` is per-read inactivity timeout, not cap on total stream duration, so long-but-progressing streaming responses are allowed to continue. Use it so stalled LLM provider surfaces clear error instead of hanging.

### Per-Model Patches

The `patches.chat_completions`, `patches.embeddings`, and `patches.rerank` fields are arrays of **jq filter strings**. Each filter receives the full request object as JSON (`{url, headers, body}`) and must return the modified version.

Filters are applied in sequence. If an expression fails, the request fails with the jq error and the name of the patch source — a skipped patch would send the very request body the patch exists to correct.

Harnx evaluates filters with [jaq](https://github.com/01mf02/jaq), which does not create missing intermediate objects the way jq does. Assign the whole container rather than a nested path:

```yaml
patches:
  chat_completions:
    - '.body.reasoning = {"effort":"high"}'      # works
    # - '.body.reasoning.effort = "high"'        # fails: `.body.reasoning` is null
```

To target specific models, use `if/then` within the expression:

```yaml
type: openai
patches:
  chat_completions:
    - 'if .body.model == "o4-mini" then .body.reasoning_effort = "low" end'
    - 'if .body.model == "o3" then .body.reasoning_effort = "medium" end'
    - 'if .body.model == "gpt-4.1" then .body.reasoning_effort = "high" end'
```

For prefix or pattern matching, use `test` within the expression:

```yaml
patches:
  chat_completions:
    - 'if (.body.model | test("gpt-5.*")) then .body.reasoning_effort = "high" end'
```

## Tool Servers (`tool_servers/`)

Tool servers provide external tools to Harnx. Native toolset servers (such as `harnx-attachment-tools`, `harnx-fs-tools`, `harnx-bash-tools`, `harnx-plans-tools`, `harnx-exa-tools`, `harnx-fetch-tools`, and `harnx-grep-tools`) run directly without a bridge wrapper, while external stdio MCP servers run via `harnx-mcp-bridge`. Each server is defined in a file under `tool_servers/` (such as `tool_servers/fs.yaml` or `tool_servers/exa.yaml`).

The **filename** (without `.yaml`) is used as the server name.

### Native Tool Server Example

```yaml
command: harnx-fs-tools    # Executable command
args:                      # Filesystem access is opt-in
  - --allow-repo-work
  - --allow-dev-tools
  - --allow-read
  - /srv/reference-data
env:                       # Environment variables
  API_KEY: "..."
description: "Filesystem access tools"
```

Filesystem and bash tool servers deny filesystem access when no allow inputs are configured. Use `--allow-read`, `--allow-write`, `--allow-exec`, or `--allow-rwx` for explicit paths, or opt into `--allow-common-default`, `--allow-dev-tools`, `--allow-repo-work`, or `--allow-all`. Filesystem tools enforce read and write access separately. See [Allowlist migration](migration-allowlist.md) for removed settings.

### External MCP Server Example (`harnx-mcp-bridge`)

To configure an external stdio MCP server, define a server in `tool_servers/` that launches `harnx-mcp-bridge`. The bundled Exa server runs natively as `harnx-exa-tools`; this example shows how to bridge an external stdio alternative:

```yaml
command: harnx-mcp-bridge
description: "Web search via Exa API"
args:
  - --name
  - exa
  - --
  - npx
  - "-y"
  - exa-mcp-server
env:
  HARNX_BASH_ENV_PASSTHROUGH: EXA_API_KEY
```

> **Passing secrets to MCP servers.** Two things commonly trip people up:
>
> - **`env:` values are literal — they are *not* shell-expanded.** `$VAR`/`${VAR}` in an `env:` value is passed through verbatim rather than substituted. Writing `API_KEY: "$API_KEY"` sends the literal string `$API_KEY` to the server.
> - **A sandbox wrapper strips the child environment.** If you've wrapped `npx`/`node` with a [harnx sandbox](sandbox-run.md), the server starts with a scrubbed environment, so neither an `env:` value nor an inherited host variable reaches it by default. Forward the specific variable with `HARNX_BASH_ENV_PASSTHROUGH` instead — the sandbox reads it and passes the host value through:
>
>   ```yaml
>   # Server needs EXA_API_KEY, and npx is sandbox-wrapped:
>   env:
>     HARNX_BASH_ENV_PASSTHROUGH: EXA_API_KEY   # forwards the host's EXA_API_KEY
>   ```
>
>   Set the actual secret (`EXA_API_KEY=…`) in `~/.local/share/harnx/.env`.

### Running Multiple Instances (`--name`)

Native tool servers register under their built-in toolset name by default. Pass
`--name <NAME>` to run another instance of the same server in one scope without
sharing its NATS subject and queue group. The override also controls the
agent-visible prefix, MCP implementation name, and telemetry service name.

```yaml
command: harnx-bash-tools
args:
  - --name
  - review
  - --allow-repo-work
```

This instance exposes tools such as `review_exec` instead of `bash_exec`. Names
must start with an ASCII letter or digit; remaining characters may also be
hyphens or underscores. The Kubernetes sandbox gateway uses `--bash-name`,
`--fs-name`, and `--sandbox-name` because it registers three toolsets.

### Restricting Published Tools (`--enable-tool`)

Tool servers publish all available tools by default. Pass `--enable-tool <glob>` in `args:` to restrict which tools a server publishes and makes callable. Tools that do not match are not registered and cannot be invoked. Repeat the flag to specify multiple patterns.

Patterns match against raw tool names before agent prefixes (for example, `read` or `exec`, not `fs_read` or `bash_exec`). Invalid glob patterns or empty values cause the server to fail at startup. Native toolset servers and `harnx-mcp-bridge` both support this argument.

```yaml
command: harnx-bash-tools
args:
  - --allow-repo-work
  - --enable-tool
  - deploy_*
  - --enable-tool
  - status
```

## NATS Servers (`nats_servers/`)

Harnx supports high-availability distributed mode via NATS. Each cluster is defined in a file like `nats_servers/local.yaml`.

The **filename** (without `.yaml`) is used as the cluster key (e.g., `agent@local`).

```yaml
url: "nats://localhost:4222" # NATS server URL
token: "${NATS_TOKEN}"       # Optional auth token
user_id: "cluster-owner"     # Optional default owner for new sessions on this cluster
tls: true                    # Enable TLS
tls_cert: "/path/to/cert"    # Optional client cert
tls_key: "/path/to/key"      # Optional client key
# tls_ca: "/path/to/ca"      # Optional CA (Note: not supported with client cert)
```

See the [NATS HA Guide](nats-ha.md) for more details.

## Example Configuration

A comprehensive reference for the new folder structure and common provider/server examples can be found in the repository at:

[**example_config/**](https://github.com/dobesv/harnx/tree/main/example_config)

This directory includes:
- `config.yaml` with recommended global settings.
- `clients/` examples for OpenAI, Claude, Gemini, Bedrock, Azure, Vertex AI, and more.
- `tool_servers/` examples for filesystem, shell, and web search.
- `agents/` examples for interactive assistants and sub-agents (auto-registered as NATS delegation toolsets).

---

## Other Settings

### RAG

See the [RAG Guide](rag-guide.md) for detailed setup instructions.

### Appearance

- **highlight**: Whether to enable syntax highlighting.
- **light_theme**: Whether to use the light theme.

### Terminal Status

Harnx signals agent status (working, blocked, done, interrupted, error) to
compatible terminals via OSC escape sequences:

- **Terminal support**: Orca renders status in panes via OSC 9999. kitty (v0.40+),
  JetBrains IDEs (2024+), and Windows Terminal render progress indicators in
  tabs/docks via OSC 9;4 (ConEmu progress). Zed is unsupported (alacritty
  backend drops OSC sequences).
- **terminal_status**: Boolean to enable/disable emission. Defaults to `true`.
  Set to `false` in `config.yaml` or via `HARNX_TERMINAL_STATUS=0` environment
  variable to disable.
- **Auto-disable**: Emission is suppressed when stdout is not a TTY,
  `TERM=dumb`, or `CI` is set in the environment.

Error state renders as a red progress bar (OSC 9;4 state=2) and uses
`"interrupted"` in the OSC 9999 JSON payload for compatibility with orcatui.

Example `config.yaml`:

```yaml
terminal_status: false  # Disable terminal status emission
```

Or via environment:

```sh
HARNX_TERMINAL_STATUS=0 harnx
```

### Loop Detection

Harnx stops an agent that keeps making the same tool call and getting the same
result. Two calls are the same when they use the same tool with the same
arguments (the order of keys does not matter) and return the same result, and
only calls from the last 10 minutes are compared. A call that returns something
new, such as a poll of a job that is making progress, is not a repeat. A poll
that keeps getting the same answer is a repeat, such as a status check that
always says "still running". `time_wait` and `time_wait_until` report when they
started and ended, and a still-running `bash_wait` reports how long the process
has been running, so their results differ from one call to the next.
Every `bash_exec` result for a command that ran carries a fresh execution id,
so a repeated `bash_exec` command is never counted. A `bash_exec` call that
fails before its command starts, such as one with an empty command, returns
the same error each time and does count.

- The 2nd to 4th identical call runs, and harnx appends a `[harnx]` note to its
  result saying how many times the call has repeated.
- The 5th is refused. It does not run, and the model gets an error that says
  when the call may run again.
- If the model's next response asks for a call that would be refused too, or
  the same call would be refused a third time, harnx ends the turn with an
  error naming the tool. A parent agent that delegated to the agent receives
  this as a `termination` of kind `"repetition"` (see the
  [Agent Guide](agent-guide.md)), and a one-shot `harnx prompt` exits with
  code 2 (see
  [Limit Exhaustion Behavior](command-line-guide.md#limit-exhaustion-behavior)).

The count covers one turn. It starts over when a message arrives mid-turn or the
session is compacted.

The guard is on by default. To turn it off everywhere, set this in
`config.yaml`:

```yaml
loop_detection:
  tool_calls: false
```

or start harnx with `HARNX_LOOP_DETECTION=0`, which takes precedence over
`config.yaml`. The guard runs in the worker that executes the turn, so a
separately deployed worker needs the setting in its own `config.yaml` or
environment. An agent can override the global setting in its front matter, for
example an agent whose job is polling:

```yaml
---
loop_detection:
  tool_calls: false
---
```

The agent's own value wins over the global one in both directions. An agent
that sets `tool_calls: true` keeps the guard on even when
`HARNX_LOOP_DETECTION=0`, and an agent that does not set it follows the global
value.

To see where the guard would have stepped in on a stored session:

```sh
harnx dump session <agent> <session-id> --check-loop-detection
```

The [Command Line Guide](command-line-guide.md) describes the output and its
limits.

#### Repeated output

Harnx also stops a model that streams the same text over and over, which some
models do in their answer or in their reasoning. When the last 2,000 or more
characters of either one are the same piece of text repeated at least four
times back to back, harnx stops the response and sends the request again with
a short note telling the model it was repeating itself. If the model repeats
itself again, harnx tries the agent's next fallback model with the same note,
and when no model is left it ends the turn. A parent agent receives this as a
`termination` of kind `"repetition"` with `source` set to `"answer"` or
`"thinking"`, and a one-shot `harnx prompt` exits with code 2. The text
streamed before a stop stays on screen, but only a reply that finished is
saved.

When a provider streams its reasoning through an OpenAI chat-completions API,
clients such as `openai-compatible` and `llama-server` put that reasoning in
the answer text, wrapped in `<think>` tags. A reasoning loop from such a
provider is still stopped, but harnx sees it as a repeating answer: the note
says the reply repeated, and `source` is `"answer"`.

Only exact repetition counts, so a list that repeats a pattern with different
values, such as numbered steps, is never stopped. Legitimate output that
repeats one block verbatim for 2,000 or more characters is stopped, though,
such as a zero-filled matrix or array initializer, an empty table or grid
template with identical rows, or identical log lines pasted into an answer. An
agent that writes output like that needs this guard turned off.

This guard is on by default too. `HARNX_LOOP_DETECTION=0` turns off both
guards. To turn off only this one, set `output: false`, in `config.yaml` or in
an agent's front matter:

```yaml
loop_detection:
  output: false
```

The `--check-loop-detection` replay described above also reports the saved
replies this guard would have stopped.

### Run Execution Limits (`run_limits`)

Autonomous runs have a finite wall-clock deadline. If no positive timeout is set, the global fallback is **86,400 seconds (24 hours)**. This is a policy default, not a measured workload threshold. Only a fresh external admission starts a new run; autonomous delegation, continuation and replay don't renew its deadline.

#### Global and Per-Agent Configuration

Set a global allowance in `config.yaml`:

```yaml
run_limits:
  timeout_secs: 86400
```

Override the local allowance for a target agent in its Markdown front matter (`<config-dir>/agents/<name>.md`):

```yaml
---
model: openai:gpt-4o
run_limits:
  timeout_secs: 7200
---
You are a helpful assistant.
```

For long work, use a longer finite value. Seven days is `604800` seconds; thirty days is `2592000`:

```yaml
run_limits:
  timeout_secs: 2592000
```

The field accepts integers, not strings. There is no deadline-disable setting. Duration and timestamp arithmetic is checked; values that overflow are rejected.

#### Precedence and Inheritance

1. A positive global `run_limits.timeout_secs` replaces the 24-hour fallback.
2. A positive target-agent timeout replaces the global allowance. The target worker's effective configuration, including package patches, is authoritative.
3. A positive delegation `timeout_secs` replaces the target's local allowance for that invocation.
4. A frozen ancestor deadline clamps the result: `min(admitted_at + local allowance, ancestor deadline)`.

At every configuration and delegation level, **omitted, `null`, zero and negative integers all mean inherit/default**. A target with any of these values uses the global allowance; a global setting with any of these values uses 24 hours. A delegation override with any of these values uses the target's effective policy. None can bypass or extend a shorter inherited deadline.

#### Frozen Admissions

`RunLimitsRecord` captures the original admission timestamp and resolved absolute deadline before execution. Config reloads apply to new admissions; active runs and worker replays retain their saved deadlines. Each newly admitted root has a finite deadline, including macro and incoming MCP request roots. Cancellation remains invocation-fenced and doesn't roll back external side effects.

### Session Titles

Harnx can automatically generate a short, human-readable title for each session
using an LLM. For NATS-backed sessions, titles and regeneration bookkeeping are
stored in canonical session metadata rather than in the conversation transcript.
Titles are shown in session listings, and the active title is also used to set
the terminal window title in the TUI and the browser tab title in the web UI (as
`harnx — <title>`).

- **title_agent**: Name of the agent used to generate titles. When unset, no
  titles are generated. Can be set globally in `config.yaml` or per-agent in an
  agent's front matter (the agent-level value takes precedence). Point it at a
  small, fast chat model.
- **title_update_threshold**: Number of tokens of growth after which the title
  is regenerated (applies both at turn end and during the tool loop). Defaults
  to `50000`. The first title is generated on the first exchange (growth from 0
  crosses any non-zero threshold). Set to `0` to disable automatic title
  generation entirely.
- **title_update_interval_secs**: Optional minimum seconds between title
  regenerations while the tool loop is running. Titles regenerate mid-loop
  whenever token growth reaches `title_update_threshold` or
  `title_update_interval_secs` seconds elapse, whichever comes first. Defaults to
  `0`, which disables only the time trigger while token-based mid-loop updates
  remain active. Set `>0` (such as `30`) to also refresh on a time cadence during
  long tool rounds that do not grow many tokens. The first title appears promptly
  by bypassing the interval. Setting `title_update_threshold: 0` disables all
  title generation, including mid-loop updates.

Example `config.yaml`:

```yaml
title_agent: title-writer      # an agent configured with a small, fast model
title_update_threshold: 50000
title_update_interval_secs: 30
```

You can also override the title agent for a specific agent in its front matter:

```yaml
---
model: openai:gpt-4o
title_agent: title-writer
---
```

**Do not configure a reasoning model** (e.g. OpenAI `o1`/`o3`, DeepSeek-R1) as
the `title_agent`. Such models spend their token budget on internal reasoning
and often return an empty or truncated title. Use a standard chat model.

To set a title manually, use the [`.set title`](tui-guide.md) command in the
TUI. A manually set title freezes automatic regeneration for the rest of the
session.

### NATS Worker Claim Timeout

- **nats_lease_acquisition_timeout_secs**: Maximum seconds a client waits for a
  worker to claim an activated NATS session. Defaults to `60` and must be a
  positive integer. This setting applies to top-level and sub-agent sessions.
  Override it with `HARNX_NATS_LEASE_ACQUISITION_TIMEOUT_SECS` when configuring
  deployments through environment variables.

```yaml
nats_lease_acquisition_timeout_secs: 60
```

### Session Retention and Garbage Collection

- **cleanup_remote_sessions_days** (env: `HARNX_CLEANUP_REMOTE_SESSIONS_DAYS`):
  Retention period in days for remote NATS sessions (integer). Defaults to unset
  (`null`), which leaves automatic session garbage collection disabled. Set `0`
  to explicitly disable collection. When set to a positive integer (such as `30`),
  `harnx-worker` daemons collect inactive sessions older than that many days during
  periodic hourly sweeps.

  Purged resources include:
  - Transcript stream: `SESSION_<sha256(id)>`
  - Leases: `harnx_leases`
  - Metadata keys: `sessions/{id}/*` in `harnx_sessions` (including read and unread tracking)
  - Invocation journal: `harnx_tool_invocations`
  - Media objects: `media/<owner>/` in `harnx_attachments`
  - Plans KV: `plan/<owner>/` in `harnx_plans`
  (where `<owner>` is `session_key(agent, id)`).

  When unset, workers emit a startup warning that automatic expiry is disabled
  and remote session state will grow unbounded.

  Read and write access to attachments (`cid:media:`) and plans (`cid:plan:`)
  refreshes the owning session's activity timestamp (`SessionActivity.last_activity_at`),
  debounced to ≤1 write/hour. This resets the retention clock so actively referenced
  media and plans avoid premature collection.

Example `config.yaml`:

```yaml
cleanup_remote_sessions_days: 30
```

### Session User Identity

Harnx can record an opaque user identity string (such as a username, email, or account ID) in canonical session metadata (`user_id`). The identity is stored once when the session is created and is never overwritten on subsequent prompts or turns.

Harnx treats user identity as an opaque string; it does not authenticate users itself. Upstream authentication proxies or callers provide the identity. `user_id` is visible in session listings to anyone who can list that agent's sessions; listing is not authorized per user. Don't configure secret-bearing cookies or headers (such as `_oauth2_proxy`, access tokens, or signed JWTs) as sources. Their values would be stored and exposed as-is.

A trusted proxy must authenticate callers and **replace, not append to**, client-supplied identity headers. Header sources use the first comma-separated value, so appending a trusted value still allows spoofing. Cookie identity values must also come from the trusted proxy. `header:NAME` and bare `NAME` select a header; `cookie:NAME` selects an exact, case-sensitive cookie name.

- **user_id** (env: `HARNX_USER_ID`, including from `.env`): Global default user identity for new sessions.
- **user_id** in `nats_servers/<cluster>.yaml`: Default user identity for new sessions created on that NATS cluster.
- **serve_user_id_sources** (env: `HARNX_SERVE_USER_ID_SOURCES`): An ordered list of HTTP headers or cookies that `harnx-serve` checks to resolve user identity from incoming requests.

#### Precedence

When a new session is created, user identity resolves in priority order:

1. **Request identity**: Resolved by `harnx-serve` from configured `serve_user_id_sources` (or `--user-id-source` flags).
2. **Cluster default**: Configured `user_id` in `nats_servers/<cluster>.yaml` for the destination cluster.
3. **Global default**: Configured `user_id` in `config.yaml`, or the `HARNX_USER_ID` environment variable / `.env` file.

Nonblank explicit or inherited identities (for example, A2A caller identities, sub-agents, or handoff-created sessions inheriting the source's `user_id`) take precedence over destination defaults. Inheritance respects the source property's inheritance flag. Blank explicit or inherited strings count as absent, so defaults apply; invalid HTTP identity sources still return 401 instead of falling back. Handoffs copy user identity, not execution-context properties such as the source branch.

Once metadata is written at session creation, the identity is immutable, including on handoff into an existing session. For concurrent creators, the first successful metadata creation wins. Promptless subscriptions and control commands don't bind session ownership.

Example `config.yaml`:

```yaml
# Global default identity for new sessions
user_id: alice

# Identity sources for harnx-serve (first match wins)
serve_user_id_sources:
  - "header:X-Forwarded-User"
  - "header:X-Forwarded-Email"
  - "cookie:session_user"

# Trusted raw group membership headers for harnx-serve (all values contribute)
serve_group_headers:
  - "X-Forwarded-Groups"

# Trusted raw role membership headers for harnx-serve (all values contribute)
serve_role_headers:
  - "X-Forwarded-Roles"
```

## Access Control (`access.yaml`)

`harnx-serve` and `harnx-a2a-server` support optional, identity-based access control rules defined in `access.yaml`.

When access rules are enabled, incoming requests are matched against the rules by caller identity. The servers decide which agents the caller can see, which sessions they can create or access, and which attachments they can retrieve. When no rules file is configured, access control is disabled and servers remain open as before.

### File Location and Options

**Restart required**: Access rules are compiled once at server startup. Modifying, adding, or removing rules requires a server restart; there is no hot-reload. To revoke access immediately, remove the rules file or update its contents and restart both `harnx-serve` and `harnx-a2a-server`.

- **Default location**: `<user-config-dir>/harnx/access.yaml`. If this file exists, access control is enabled automatically. If it does not exist, access control remains disabled.
- **Explicit path**: Pass `--access-rules <PATH>` on the CLI or set the `HARNX_ACCESS_RULES` environment variable. When an explicit path is provided, the file must exist; startup fails if the file is missing or invalid.
- **Identity source requirement**: When access rules are enabled, servers require a trusted identity source (`serve_user_id_sources` or `--user-id-source` in `harnx-serve`, `--user-id-header` in `harnx-a2a-server`). If rules are enabled without configured identity sources, startup fails immediately.

### Rule Format

An `access.yaml` file defines a list of rules under the `rules` key:

```yaml
rules:
  - agents:
      - sisyphus
      - daedalus
      - "coding/*"
    scopes:
      - prompt
    users:
      - "alice@example.com"
      - "bob@example.com"
```

Each rule contains:
- **`agents`** *(required list of strings)*: Globs matching agent references.
- **`scopes`** *(optional list of strings)*: Granted permissions. Allowed values are `prompt` and `admin`. If omitted, defaults to `[prompt]`. An empty list (`scopes: []`) is a configuration error.
- **`users`** *(optional list of strings)*: Globs matching caller user identity strings. Omitted or empty matches nothing.
- **`groups`** *(optional list of strings)*: Globs matching caller group memberships. Omitted or empty matches nothing.
- **`roles`** *(optional list of strings)*: Globs matching caller role memberships. Omitted or empty matches nothing.

At least one selector category (`users`, `groups`, or `roles`) must be non-empty. Unknown YAML fields are rejected (`deny_unknown_fields`).

#### Group and Role Memberships

In addition to user identity, callers can supply group and role memberships via trusted HTTP headers configured on the server:
- `harnx-serve`: `serve_group_headers` / `serve_role_headers` in `config.yaml`, environment variables `HARNX_SERVE_GROUP_HEADERS` / `HARNX_SERVE_ROLE_HEADERS` (comma-separated header names), or repeatable `--group-header <NAME>` / `--role-header <NAME>` CLI flags. Non-empty CLI flags replace config and environment settings.
- `harnx-a2a-server`: repeatable `--group-header <NAME>` and `--role-header <NAME>` CLI flags.

Header and membership rules:
- **No defaults**: No trusted group or role headers are enabled by default. Unconfigured membership headers sent by clients are ignored.
- **Header parsing**: Every configured header name is read from the request. When a header appears multiple times or contains comma-separated values, each comma-separated entry is trimmed and empty tokens are dropped. All configured membership headers contribute values.
- **Fail closed on malformed values**: If any configured membership header contains invalid bytes that fail UTF-8 header value conversion, the request is rejected immediately with HTTP 401 Unauthorized without echoing header contents.
- **Distinct namespaces**: Users, groups, and roles occupy completely distinct namespaces. The same string value in different namespaces never matches across categories.
- **OR within rules, union across rules**: A rule matches when its `agents` pattern matches AND at least one selector category (`users`, `groups`, or `roles`) matches the caller's request. Scopes are unioned across all matching rules.
- **Request-local and non-persistent**: Groups and roles are evaluated per request and are never persisted in session metadata, task KV, or NATS properties. Revoking or changing a membership takes effect on the caller's next HTTP request.
- **User ownership unchanged**: Groups and roles grant access scopes, but they never satisfy session ownership. Sessions are owned solely by immutable user IDs. Callers with `prompt` scope can only access sessions where `user_id == caller.user_id`. Callers with `admin` scope (granted by any user, group, or role rule) can access sessions across all users.
- **User identity required**: Memberships do not satisfy required caller authentication. When access rules are enabled, protected routes still require a valid user identity.
- **Reverse proxy trust**: Upstream proxies must authenticate callers and replace or strip all configured user, group, and role headers before forwarding requests.

#### Reusable Groups with YAML Anchors

Standard YAML anchors (`&name`) and aliases (`*name`) allow defining reusable user lists and agent groups within the `rules` list:

```yaml
rules:
  - agents: &core_agents
      - sisyphus
      - daedalus
    scopes:
      - prompt
    users: &engineers
      - "alice@example.com"
      - "bob@example.com"

  - agents: *core_agents
    scopes:
      - admin
    users:
      - "lead@example.com"
```

### Glob Matching Semantics

- **Case-sensitive**: Both `agents` and `users` patterns are case-sensitive.
- **Whole string**: Patterns match the entire identity or agent string, not substrings.
- **Wildcard spans commas**: The `*` wildcard matches any sequence of characters, including commas (`,`) and slashes (`/`).
- **Quoting DN strings**: When matching LDAP/X.500 Distinguished Names containing commas, wrap the pattern in quotes:
  ```yaml
  users:
    - "CN=*,OU=engineering,O=mycorp"
    - "CN=alice,OU=dev,O=mycorp"
  ```

### Agent References

- **`harnx-serve`**: Evaluated against the client display reference:
  - Bare name (`sisyphus`) for agents on the default cluster.
  - Suffix form (`sisyphus@shared`) for agents on remote clusters.
- **`harnx-a2a-server`**: Evaluated against the internal agent reference (`Export::agent_ref()`, e.g. `coder` or `coder@cluster`), **never** against public export aliases (such as `alias=coder`). This ensures a single `access.yaml` file functions consistently across both servers.

### Scopes and Permissions

Caller scopes on an agent are the union of all matching rules. A request matching no rules receives no scopes.

| Scope | Granted Permissions |
|---|---|
| `prompt` | View the agent; create new sessions; list, read, prompt, and manage sessions owned by the caller (`user_id == caller`). |
| `admin` | View the agent; list, read, prompt, update metadata, compact, and cancel sessions owned by any user, as well as legacy sessions without an owner. Does **not** grant permission to create new sessions. |

Holding either `prompt` or `admin` makes the agent visible in agent listings and detail routes. Creating a new session or context always requires `prompt` scope. Callers with only `admin` scope cannot create sessions.

### HTTP and JSON-RPC Status Responses

When access rules are enabled:

- **Missing caller identity**: HTTP `401 Unauthorized` (JSON error payload `{ "error": { "message": "missing user identity", ... } }` in `harnx-serve`, or JSON-RPC code `-32000` in A2A).
- **Hidden agent**: An agent on which the caller holds no scopes returns HTTP `404 Not Found` (or JSON-RPC `-32001` `task not found` in A2A). The response is identical to an unknown agent, preventing enumeration.
- **Foreign or forbidden session**: Accessing a session owned by another user without `admin` scope returns HTTP `404 Not Found` (or JSON-RPC `-32001`).
- **Session creation without `prompt`**: Attempting to create a session with only `admin` scope returns HTTP `403 Forbidden` (`session creation requires prompt scope`) in `harnx-serve`, or JSON-RPC code `-32010` in `harnx-a2a-server`.
- **Pre-allocation required in harnx-serve**: When access rules are active, callers must create and reserve a session ID via `POST /v1/agents/{agent}/sessions` before prompting. Implicit session creation on unreserved session IDs returns HTTP `404 Not Found`.

### CID Attachments and Plans (`/v1/cid/*`)

Attachments and plans addressed by canonical `cid:` URLs embed an owning session reference. When access rules are enabled:
- The server checks the caller's session permissions before resolving the blob or updating activity timestamps.
- Unauthorized callers receive HTTP `404 Not Found` (with no ETag leaked).
- Temp or inline sessions without a named agent (`SessionRef.agent == None`) return `404`.
- **Cache policy**: Responses emit `Cache-Control: private, no-store`.
- **Cache purge recommendation**: Enabling access rules does not invalidate entries previously cached by shared reverse proxies or CDNs. When turning on access rules in an existing deployment, purge shared HTTP caches for `/v1/cid/*`.

### Routes Left Open

Access rules protect `/v1/agents` and `/v1/cid/*` routes. Other endpoints remain open:
- Health and readiness endpoints (`/healthz`, `/readyz`).
- Model listings and utility endpoints (`/v1/models`, `/v1/embeddings`, `/v1/rerank`).
- Static Web UI assets and the browser SPA shell.
- Preflight CORS requests (`OPTIONS`).

### Not Yet Supported

- **`harnx-mcp-server`**: Access rules apply only to `harnx-serve` and `harnx-a2a-server`.
- **ALB JWT payload decoding**: Decoding signed `x-amzn-oidc-data` JWT claims into user DNs is deferred; use the raw identity header `x-amzn-oidc-identity`.

## User Aliases (`users.yaml`)

`harnx-serve` and `harnx-a2a-server` support optional user alias configuration defined in `users.yaml`.

User aliases expand an authenticated caller's identity into an alias group's identities for HTTP and A2A access checks, agent permissions, and session access, without rewriting stored session ownership or A2A binding records.

### Location and Lifecycle

- **Default location**: `<user-config-dir>/harnx/users.yaml`. Discovered via standard configuration directory resolution (`config_paths::local_path("users.yaml")`). Standard global configuration directory controls—such as `HARNX_CONFIG_DIR`, `XDG_CONFIG_HOME`, or `--config-dir` in `harnx-a2a-server`—apply normally to locate `users.yaml`.
- **No alias-specific overrides**: There are no alias-specific CLI flags (such as `--users-file`) or dedicated environment variables (such as `HARNX_USERS_FILE`) for specifying an alternative `users.yaml` path.
- **Optional startup-only loading**: The file is loaded once at server startup. If `users.yaml` is missing, alias expansion is skipped and each caller retains their single authenticated identity.
- **Fail-on-invalid behavior**: If `users.yaml` exists but is invalid (e.g. malformed YAML, empty file, whitespace-only, comment-only, null document, non-sequence root, missing required fields, or unknown fields), the server fails startup immediately with path context.
- **No live reload**: Modifying `users.yaml` requires restarting `harnx-serve` and `harnx-a2a-server`. Runtime edits have no effect on running servers.

### Security and Authorization

Alias entries directly alter access control boundaries: mapping caller identity `alice` into an entry containing `bob` allows `alice` to access agents and sessions permitted to `bob`.

**Trusted operator edit only**: Because alias mappings grant authorization, `users.yaml` must be protected with appropriate filesystem permissions and modified only by trusted operators.

### Schema and Format

The configuration file must be a top-level YAML sequence of entries. Mapping roots, scalar roots, null documents, and empty documents are rejected.

Each entry is a YAML mapping with two required fields:

| Field | Type | Description |
|---|---|---|
| `name` | String | Human-readable label for the alias entry. Used for display and documentation only; **never** treated as an authenticated principal or caller identity. |
| `identities` | List of strings | Set of user identity strings associated with this alias. An empty list (`[]`) is valid and acts as a no-op entry. |

Unknown fields in an entry are rejected (`deny_unknown_fields`).

#### Example `users.yaml`

```yaml
- name: Core Team
  identities:
    - "alice@example.com"
    - "alice-corp"
    - "alice-dev"

- name: Operations Team
  identities:
    - "bob@example.com"
    - "bob-ops"

- name: Cross-Department Contractor
  identities:
    - "contractor@example.com"
```

An empty sequence (`[]`) is valid. Duplicate identities within an entry (e.g. `[alice, bob, bob]`) are silently accepted without error.

### Resolution Semantics

When an authenticated caller makes a request, the server expands the caller's identity using the following rules:

1. **Ordered scan**: The server scans `users.yaml` entries in order from top to bottom.
2. **First-match wins**: The lookup stops at the **first** entry whose `identities` list contains the exact caller identity string. The caller's effective identities expand to all strings in that entry's `identities` list.
3. **Exact and case-sensitive**: Identity matching is exact and case-sensitive. No normalization, trimming, or lowercasing is applied.
4. **No transitive union**: Overlapping entries do not merge. If entry 1 contains `[alice, bob]` and entry 2 contains `[bob, carol]`, caller `bob` matches entry 1 and receives `[alice, bob]`. Entry 2 is ignored for `bob`.
5. **Unmatched callers**: If a caller's identity does not match any entry in `users.yaml`, the caller retains their singleton authenticated identity.
6. **Anonymous callers**: An unauthenticated or anonymous caller (`None`) is never expanded against `users.yaml` and remains anonymous.

### Integration with Servers and Access Control

#### Caller expansion only, stored ownership preserved
Alias expansion applies **only** to the incoming caller during request authorization:
- In `harnx-serve`, new sessions created via `POST /v1/agents/{agent}/sessions` record the caller's raw incoming identity (`user_id`) in session metadata, not an alias entry name or other identities in the group.
- In `harnx-a2a-server`, new contexts and tasks record the caller's raw incoming identity as the stored owner in the A2A binding. Stored owner identities are never expanded.
- When an authorized caller later accesses or resumes a session, their expanded caller identities are checked against the stored session owner.

#### Behavior without access rules
- **`harnx-serve`**: When access rules (`access.yaml`) are not configured, `harnx-serve` operates without access restrictions. Sessions and agents remain open, and `users.yaml` does not restrict requests.
- **`harnx-a2a-server`**: When access rules are absent, `harnx-a2a-server` still enforces owner isolation on stored bindings. A caller is permitted to access a binding if the stored owner is contained in the caller's expanded identities. Anonymous access (`None` stored owner and `None` caller) succeeds. Mixed caller/stored ownership (one anonymous, one authenticated) is denied.

#### Groups and roles unchanged
`users.yaml` expands only user identities. Request-local group headers (`--group-header`, `serve_group_headers`) and role headers (`--role-header`, `serve_role_headers`) are never expanded or modified by `users.yaml`. User aliases do not grant group or role memberships.
