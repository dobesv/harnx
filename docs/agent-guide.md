# Agent Guide

## What is an Agent?

An agent is a Markdown file that combines a system prompt with model configuration, tools, variables, documents, and hooks. Agents are the core building block for tailoring Harnx to your workflow.

Each agent lives at:

```
<harnx-config-dir>/agents/<name>.md
```

An agent can also have a companion data directory at `<harnx-config-dir>/agents/<name>/` for storing related files (like variable source files or documents).

## Agent File Format

An agent file has two parts: YAML front-matter (configuration) and a Markdown body (the system prompt).

Here's a complete example showing all available front-matter fields:

```markdown
---
model: openai:gpt-4o
temperature: 0
top_p: 0.9
use_tools:
  - fs_*
  - bash_exec
description: A helpful coding assistant
version: "1.0"
instructions: null

variables:
  - name: project_dir
    description: The project directory
    default: "."
  - name: conventions
    description: Project coding conventions
    path: conventions.md

conversation_starters:
  - What can you help me with?
  - Let's debug this issue

documents:
  - docs/architecture.md
  - docs/api-reference.md

hooks:
  max_resume: 3
  entries:
    - command: harnx-claude-compatible-hook-server --event Stop -- /path/to/hook.sh
---

You are a helpful coding assistant working on the {{project_dir}} project.

Follow these conventions:
{{conventions}}

The current OS is {{__os__}} and the shell is {{__shell__}}.
```

## Front-matter Fields Reference

| Field | Type | Default | Description |
|---|---|---|---|
| `model` | `string` | global default | LLM model ID (e.g. `openai:gpt-4o`, `claude:claude-3-5-sonnet`). If omitted, uses the globally configured model. |
| `role` | `string` | `assistant` | Agent purpose: `assistant` (interactive agent shown in menus), `subagent` (internal agent invoked via delegation), or `compaction` (session compaction agent). |
| `temperature` | `float` | global default | Controls randomness (0 = deterministic, 1 = creative). Inherited from global config when `model` is omitted. |
| `top_p` | `float` | global default | Nucleus sampling parameter. Alternative to temperature. Inherited from global config when `model` is omitted. |
| `use_tools` | `list` | none | YAML list of tool specifiers. Also accepts a comma-separated string for backward compatibility. See [Tools](#tools). |
| `description` | `string` | `""` | Short description shown in agent listings. |
| `version` | `string` | `""` | Version identifier for the agent. |
| `variables` | `list` | `[]` | Variables prompted on first use. See [Variables](#variables). |
| `conversation_starters` | `list` | `[]` | Suggested prompts shown when starting the agent in TUI mode. |
| `documents` | `list` | `[]` | Document paths for RAG integration. See [Documents](#documents-rag). |
| `instructions` | `string` | none | If set, overrides the Markdown body as the system prompt. |
| `hooks` | `object` | none | Per-agent hook configuration. See [Hooks](#hooks). |

## Variables

Variables make agents reusable by injecting dynamic values into the system prompt. They're defined in the `variables` front-matter field.

### Variable Fields

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | `string` | yes | Variable name. Used as `{{name}}` in the prompt. |
| `description` | `string` | yes | Shown to the user when prompting for a value. |
| `default` | `string` | no | Default value if the user doesn't provide one. |
| `path` | `string` | no | Path to a file whose contents become the variable's value. |

### How Variables are Resolved

When an agent starts, each variable's value is determined in this order:

1. **CLI argument** — passed via `--agent-variable name=value`
2. **File content** — if `path` is set, the file is read and its content becomes the value
3. **Default** — the `default` field value
4. **User prompt** — if none of the above provide a value, the user is prompted interactively

### File-sourced Variables

The `path` field lets you load a variable's value from a file. The path is resolved relative to the agent file's parent directory (`<config-dir>/agents/`).

```yaml
variables:
  - name: conventions
    description: Project coding conventions
    path: my-agent/conventions.md
```

This reads `<config-dir>/agents/my-agent/conventions.md` and uses its content as the variable value.

Constraints:
- The path must be relative (no absolute paths)
- Directory traversal with `..` is not allowed
- If both `path` and `default` are set, `path` takes priority (a warning is logged)

### Using Variables in Prompts

Reference variables with double-brace syntax:

```markdown
You are an expert {{language}} developer. Write clean, idiomatic {{language}} code.
```

Variables are interpolated in the system prompt (or `instructions` if set) before it's sent to the LLM.

## Built-in Variables

Harnx provides built-in variables that are always available, without needing to declare them. They use double-underscore naming:

| Variable | Description | Example Value |
|---|---|---|
| `{{__os__}}` | Operating system name | `linux`, `macos`, `windows` |
| `{{__os_distro__}}` | OS distribution details | `Ubuntu 22.04 (linux)`, `macOS 14.0` |
| `{{__os_family__}}` | OS family | `unix`, `windows` |
| `{{__arch__}}` | CPU architecture | `x86_64`, `aarch64` |
| `{{__shell__}}` | Current shell | `bash`, `zsh`, `powershell` |
| `{{__locale__}}` | System locale | `en-US`, `ja-JP` |
| `{{__now__}}` | Current date and time | `2025-01-15 14:30:00` |
| `{{__cwd__}}` | Current working directory | `/home/user/project` |

Built-in variables are interpolated after custom variables, so they work everywhere custom variables do.

## Prompt Body

The Markdown body below the front-matter `---` fence is the agent's system prompt. It's sent as a `system` role message to the LLM, with the user's input sent separately as a `user` message.

```markdown
---
model: openai:gpt-4o
---
You are a helpful assistant that explains things clearly and concisely.
```

Running `harnx prompt -a my-agent "What is Rust?"` produces these messages:

```json
[
  {"role": "system", "content": "You are a helpful assistant that explains things clearly and concisely."},
  {"role": "user", "content": "What is Rust?"}
]
```

If the body is empty, no system message is generated and only the user message is sent.

The `instructions` front-matter field, if set, overrides the body entirely. This is useful when you want to set the prompt programmatically or keep the body as documentation while using a different prompt at runtime.

Both the body and `instructions` support `{{variable}}` and `{{__builtin__}}` interpolation.

### Compaction Prompt Design

The `coding` and `pantheon` packages and `example-compaction-agent` ask for a
continuation checkpoint: enough state to resume the work, with exact references
and explicit progress. Their shared guidance keeps plan/task IDs and URLs,
important changed files, decisions and rationale, verification evidence, and
pending work. Specialist prompts add their own evidence and verdict requirements.
Headings are adaptable; completeness of relevant state matters more than filling
a fixed report template. Large logs are condensed to results and diagnostic
excerpts, while constraints and unfinished work survive repeated compactions.

This design draws on three primary sources reviewed for [issue #2495](https://github.com/dobesv/harnx/issues/2495):

- [Anthropic's context engineering guidance](https://www.anthropic.com/research/effective-context-engineering-for-ai-agents)
  recommends tuning compaction for recall first, preserving decisions and
  unresolved work while removing redundant output.
- [Pi's compaction prompts](https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/src/core/compaction/compaction.ts)
  distinguish initial summaries from updates and retain constraints, progress,
  decisions, and next steps across updates.
- [OpenCode's compaction prompts](https://github.com/anomalyco/opencode/blob/dev/packages/core/src/session/compaction.ts)
  preserve exact references and consequential workflow state, reconcile older
  checkpoints with newer evidence, and separate summarization from task execution.

Harnx supplies a sanitized, budgeted transcript as the compaction request's input
(`config/compaction/request.rs` and `transcript.rs` in `harnx-runtime`); no
`{conversation_history}` placeholder is needed in the agent prompt. Kept messages
are excluded from that transcript. Tool results may already be shortened, so a
prompt cannot recover missing details; it should identify consequential gaps
instead of inventing them. The built-in fallback used without a configured
`compaction_agent` is separate and still requests about 200 words.

When evaluating prompt changes, use transcripts with an earlier summary, a
partially completed plan, changed files, a failed check, and a later user
correction. Check that the output retains exact references and their purpose,
updates statuses without reviving resolved blockers, distinguishes proposed work
from verified results, and identifies the next action. Include a mid-task cutoff
and a second compaction to check continuity. Prompt-rendering tests validate
loading and the request/reply instruction; they do not establish model recall.

## Tools

The `use_tools` field controls which MCP tools the agent can access. Tools are specified as a YAML list (a comma-separated string is also accepted for backward compatibility). Glob patterns are supported via the `globset` crate, including `*` wildcards and `{a,b}` brace expansion.

### Syntax

| Pattern | Meaning |
|---|---|
| `tool_name` | Enable a single tool by name |
| `server_*` | Enable all tools from an MCP server (glob pattern) |
| `*` | Enable every available tool |
| `prefix_{a,b}` | Enable specific tools matching a brace expansion |
| `toolset_name` | Enable a named toolset (defined in global config) |

Tool names follow the pattern `{server}_{tool}`, where `server` is the server's registered name. Bundled configs are named to match, so `tool_servers/attachments.yaml` gives `attachments_attachment_read` and `attachments_attachment_create`.

When editing `use_tools` lists, match the list's indentation exactly: some agents use column 0, others use 2-space indent. Wrong indentation folds entries into the previous line's value; YAML still parses but tools silently disappear.

### Examples

```yaml
# Single tools
use_tools:
  - web_search
  - execute_command

# All tools from a server
use_tools:
  - fs_*
  - git_*

# Everything
use_tools:
  - "*"

# Mix of patterns
use_tools:
  - fs_*
  - web_search
  - my_toolset

# Specific tools via brace expansion
use_tools:
  - fs_{read_file,write_file,list_directory}
```

When tools are enabled, their declarations are injected into the system prompt as a numbered list appended after the prompt body.

### Attachment Tools

The `harnx-attachment-tools` server provides tools for reading and storing NATS-backed media and plan documents:

- **`attachment_read(url, ...)`**: Read an attachment or rendered plan by its `cid:` URL (`cid:media:` or `cid:plan:`).
  - Returns image blocks for images, formatted text with line numbers for text media and rendered plans, and errors for unsupported binary formats.
  - Parameters:
    - `url` (string, required): The canonical `cid:` URL to read.
    - `offset` (integer, optional): Line number to start reading from (1-indexed).
    - `limit` (integer, optional): Maximum number of lines to return from offset.
    - `head_lines` (integer, optional): Return only the first N lines.
    - `tail_lines` (integer, optional): Return only the last N lines.
    - `max_output_bytes` (integer, optional): Maximum output size in bytes.
    - `grep` (string, optional): Regex pattern to filter lines before truncation.
- **`attachment_create(content, mime_type)`**: Create a new text attachment in NATS Object Storage (`harnx_attachments`) owned by the invoking session.
  - Returns the canonical `cid:media:` URL for the stored attachment.
  - Parameters:
    - `content` (string, required): The text content to store.
    - `mime_type` (string, required): Text MIME type (e.g. `text/plain`, `text/markdown`, `application/json`, `application/xml`, `application/yaml`). Non-text MIME types are rejected.
  - Requires caller session identity; returns an error when invoked without session context.

> **Note**: Agents list these tools in `use_tools` as `attachments_attachment_read` and `attachments_attachment_create`. The prefix (`attachments`) is the server's registered name, which by convention matches `tool_servers/attachments.yaml`.

### Session Metadata Tools

Two built-in tools let an agent see and record what its own session is working on. Harnx answers them itself, so they need no tool server, but an agent only gets them by listing them in `use_tools` (or by selecting everything with `*`):

```yaml
use_tools:
  - harnx_read_session_meta
  - harnx_write_session_meta
```

- **`harnx_read_session_meta()`** returns the session ID, agent, title and creation time, the session's properties, and `observed_repositories`: the repositories and branches the session's tool calls have worked in.
- **`harnx_write_session_meta(set, clear, add_labels, remove_labels)`** changes properties and returns the same view afterwards. `clear` runs first, then `set`, then `remove_labels`, then `add_labels`.

Each property holds a value and an `inherit` flag. A sub-agent session started from this one copies the properties marked `inherit`, so a delegated child knows the issue and branch it is working on. Harnx knows these properties:

| Property | Value | Copied to sub-agent sessions |
|---|---|---|
| `user_id` | The user the session acts for. Read-only for agents; Harnx doesn't record one yet. | by default |
| `github_owner_repo` | GitHub repository as `owner/repo` | by default |
| `git_branch` | Git branch | by default |
| `github_issue` | Issue number in `github_owner_repo` | by default |
| `github_pull_request` | Pull request number in `github_owner_repo` | by default |
| `external_task_url` | http(s) URL of the task in another tracker, such as Jira or Linear | by default |
| `working_directory` | Directory the session works in | by default |
| `web_session_url` | http(s) URL that opens the session in the Web UI. harnx-serve or a worker fills it in when configured with `serve_public_url`; see [Session properties](nats-ha.md#session-properties). | never |
| `labels` | List of labels | on request |

Any other name that starts with a lowercase letter and uses only lowercase letters, digits, `_`, `-` and `.` is a custom property holding text, copied only on request. A session keeps at most 32 custom properties and 50 labels.

```json
{
  "set": [
    {"name": "github_owner_repo", "value": "dobesv/harnx"},
    {"name": "github_issue", "value": 2296},
    {"name": "customer", "value": "acme", "inherit": true}
  ],
  "add_labels": ["in-progress"]
}
```

A blank value, `0` for a number, or an empty list removes a property. When `inherit` is omitted an existing property keeps its flag and a new one takes the default above; asking to copy `web_session_url` is an error. Writing a property only records it: it doesn't check out a branch, change directory or contact GitHub.

## Documents (RAG)

The `documents` field lists files or URLs to include as retrieval-augmented generation (RAG) context. When an agent with documents starts, Harnx offers to initialize a RAG index.

```yaml
documents:
  - docs/architecture.md
  - docs/api-reference.md
  - https://example.com/guide.html
```

Relative paths are resolved from the agent's data directory.

## Hooks

Hooks let you run external commands at specific points during agent execution. They're configured under the `hooks` front-matter field. For a complete reference, see [Hooks Guide](hooks-guide.md).

### Configuration

```yaml
hooks:
  max_resume: 3
  entries:
    - command: >-
        harnx-claude-compatible-hook-server
        --event PreToolUse
        --matcher shell
        --timeout 15
        -- /path/to/approve-tool.sh
    - command: harnx-claude-compatible-hook-server --event Stop -- /path/to/on-stop.sh
      status_message: "Running stop hook..."
      async: true
```

### Hook Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `command` | `string` | yes | — | Shell command to run as a hook server. For hooks that need event/matcher, use `harnx-claude-compatible-hook-server --event <E> --matcher <M>` with either `--jaq <FILTER>` or `-- <child-command>`. |
| `status_message` | `string` | no | none | Message to display while the hook runs |
| `async` | `boolean` | no | none | Whether to run the hook asynchronously |

The `command` field specifies a hook server binary. Options:
- **Generic runner**: `harnx-claude-compatible-hook-server --event <EVENT> [--matcher <REGEX>] [--timeout <SECS>] [--priority <N>] [--fail-policy <closed|open>] (--jaq <FILTER> | [--persistent] -- <child-command>)`. `--jaq` uses Harnx's embedded jaq engine; `--persistent` keeps a child command alive across requests.
- **Native hooks** (e.g., `harnx-proxy-auth`): Self-declare their event/matcher and need no runner flags.

### Top-level Hook Settings

| Field | Type | Description |
|---|---|---|
| `max_resume` | `integer` | Maximum number of resume iterations |

### Merge Behavior

Agent hooks extend global hooks (defined in the main config). The merge rules are:

- Agent entries are combined with global entries
- If an agent entry has the same `event` and `matcher` as a global entry, the agent entry replaces it
- `max_resume`: agent value overrides global if set; otherwise the global value is used

## Using Agents

### From the Command Line

```sh
harnx --agent <name>                    # Start an agent
harnx prompt --agent <name> "your question" # Run a non-interactive prompt
harnx --list-agents                     # List available agents
```

You can also pass variable values directly:

```sh
harnx prompt --agent coder --agent-variable language rust "write a web server"
```

### From the TUI

```
.agent <name>        Switch to an agent
.info agent [<name>] Show fully-rendered agent config (interpolated)
.edit agent          Edit the agent's .md file
.save agent [name]   Save current agent configuration
```

### Inline Prompt

Use `--prompt` to create a temporary agent without a file:

```sh
harnx prompt --prompt "You are a helpful translator" "translate hello to French"
```

## Agent Handoffs

An agent can permanently transfer a conversation to another assistant with a
synthetic `{agent}_session_handoff` tool. A handoff is different from nested
delegation: the source turn finishes immediately after the target prompt is
durably queued, and the target runs as an ordinary top-level NATS session. The
source does not wait for the target's response.

The optional `session_id` follows the same rules as `{agent}_session_prompt`:

- Omit it, or pass an empty/whitespace value, to generate a new target session.
- Pass an unused ID to create that exact target session.
- Pass an existing session owned by the target agent to continue its transcript.
- IDs are local to the target agent: another agent can use the same ID independently.
  For example, two review agents can each use `review-12345` without sharing history.

Do not invent an ID when a generated session is desired. When omitted, the
tool reserves the new target session ID during tool execution and dispatches
to that same ID. The tool result carries the target agent and child session ID
(text `"Handing off to <agent>; target session: <id>."` and a structured
`handoff: {agent, session_id}` marker without a user-only audience restriction),
making the target identity visible in both parent model context and session
dumps. Web and TUI interfaces render a static navigable row pointing to the
child session. The committed target ID is confirmed after the prompt is
persisted and worker activation is published. Bare and package-qualified
targets stay on the current cluster; explicit `agent@cluster` targets use that
configured cluster.

The source session keeps its agent configuration, history, hooks, persistence
backend, and lease. The target's normal activation independently loads its
metadata, acquires its lease, reconciles its hooks, and drains the queued turn.
Web and TUI clients follow the confirmed target; one-shot CLI output reports the
handoff without automatically following it.

## Sub-Agents & Agent Delegation

Harnx supports agent delegation, allowing a parent agent to run nested sub-agents for specialized tasks (such as code analysis, research, or execution planning).

### NATS-Based Session Model

Sub-agents in Harnx execute as standard NATS agent sessions (`NatsSession`). ACP (Agent Client Protocol) and its stdio child process architecture have been removed.

- **Markdown-only agent definitions**: Agents are defined solely by Markdown files with YAML front-matter in `<config-dir>/agents/*.md` (or package agents). ACP server configuration (`acp_servers/*.yaml`) and ACP stdio child processes no longer exist.
- **Auto-registered toolsets**: For every configured agent, the worker daemon registers a NATS-backed 4-tool toolset. Each registration advertises the raw names `session_new`, `session_prompt`, `session_load`, and `session_cancel`; the provider exposes them to agents with an agent-relative prefix:
  - `{agent}_session_new`: Creates a new sub-agent session and returns its initial response along with session metadata.
  - `{agent}_session_prompt`: Sends a prompt message (`message`, optional `attachments`, optional `session_id`, optional `timeout_secs`) to a sub-agent session, returning the sub-agent's final response text or a synthesized termination result. `attachments` is a list of `cid:` attachment URLs passed to the sub-agent; repeated URLs are ignored. The parent session ID is propagated internally.
  - `{agent}_session_load`: Reads prior event history for an existing sub-agent session log.
  - `{agent}_session_cancel`: Cancels an in-flight prompt on a sub-agent session.
- **Route-aware execution**: On persistent clusters, sub-agent turns use the
  cluster-shared JetStream work queue (`WORK_NOTIFY_<cluster>`) and may run on
  any available worker. A frontend-owned local worker targets nested
  activations back to its own worker ID. Distributed `harnx_leases` still
  ensure exactly one active holder per session in either topology.
- **Lease-backed liveness**: Sub-agent tool calls run synchronously from the
  parent agent's perspective without an implicit idle or elapsed-time deadline.
  The child worker renews its session lease independently of model and tool
  activity; if that worker disappears without writing a durable result, the
  waiting session detects the expired lease and returns an error. Parent aborts
  and explicit `session_cancel` calls still cancel the child turn. TUI child
  rows also refresh durable history and sample the same lease, so a lost worker
  becomes failed instead of displaying a running spinner indefinitely.

### Sub-Agent Tool Result Marker

When `{agent}_session_new` or `{agent}_session_prompt` completes, the tool returns a JSON object containing the sub-agent response text, the existing structured identification marker (`sub_agent`), and the invocation's terminal progress snapshot (`sub_agent_progress`):

```json
{
  "session_id": "01948a3f-7b1c-7123-8901-abcdef123456",
  "response": "Analysis complete. Here are the findings...",
  "sub_agent": {
    "agent": "researcher",
    "session_id": "01948a3f-7b1c-7123-8901-abcdef123456"
  },
  "sub_agent_progress": {
    "invocation_id": "8aa9a68a-034e-4df3-a9cf-6db978644f30",
    "agent": "researcher",
    "session_id": "01948a3f-7b1c-7123-8901-abcdef123456",
    "status": "done",
    "elapsed_ms": 12430,
    "usage": {
      "input_tokens": 1480,
      "output_tokens": 392,
      "cached_tokens": 960
    },
    "tool_call_count": 3
  }
}
```

The `sub_agent` marker keeps its original wire shape and carries the exact
`AgentSource` identity (`agent`, `session_id`). `sub_agent_progress` lets
clients restore terminal metrics from durable session history. Metrics are
scoped to one delegation invocation, even when several invocations reuse the
same child session. Token usage accumulates every model call made directly by
that child; tool count includes tools started directly by it. Nested agents'
model and tool events stay on their own progress rows and are not double
counted in the parent invocation.

If the call does not succeed, because the child's turn fails, the tool server
is lost, the parent is interrupted, or a worker restart loses the response, its
error output carries the child's identity as a `partial_result`:

```json
{
  "is_error": true,
  "error": "tool server unavailable: ...",
  "partial_result": {
    "session_id": "01948a3f-7b1c-7123-8901-abcdef123456",
    "sub_agent": {
      "agent": "researcher",
      "session_id": "01948a3f-7b1c-7123-8901-abcdef123456"
    }
  }
}
```

The parent can inspect that session or prompt it again.

### Run Limits & Sub-Agent Termination

Harnx bounds autonomous agent executions and runaway loops using runtime-owned wall-clock deadlines. Generated delegation tool metadata describes the target's allowance and when to set an override. Stopped invocations return continuation advice with available public results; shared agent prompts don't need run-limit instructions. CLI `--token-budget` remains an independent accounting limit for non-interactive one-shot prompts.

#### Delegation Parameters and Policy Precedence

When delegating via `{agent}_session_prompt`, callers can pass:
- `message` (required string): The prompt message for the sub-agent.
- `attachments` (optional array of strings): Canonical `cid:` URLs.
- `session_id` (optional string): Prior session ID to continue, or omitted to start a fresh session.
- `timeout_secs` (optional integer, seconds): Per-call timeout override.

Effective deadlines follow a four-tier precedence hierarchy:

1. **Global configuration**: `run_limits.timeout_secs` set in `config.yaml`.
2. **Target agent configuration**: `run_limits.timeout_secs` set in the target agent's Markdown front matter. The publishing target worker's effective configuration (including package patches) is authoritative; target policy is never inferred from caller heuristics.
3. **Call-level override**: `timeout_secs` passed in `{agent}_session_prompt`.
4. **Ancestor deadline clamp**: `min(admitted_at + local allowance, ancestor deadline)`. Target configuration and call overrides cannot extend a frozen ancestor deadline.

The `timeout_secs` field accepts integers, not strings:
- **Omitted, `null`, zero or negative integer**: Inherits the next policy level. Delegation uses the target's effective policy; the target inherits global configuration. With no positive configuration, the finite fallback is **86,400 seconds (24 hours)**.
- **Positive integer**: Sets an explicit finite local allowance in seconds, clamped by any ancestor deadline. Large values such as `604800` (7 days) or `2592000` (30 days) are allowed; overflow is rejected.

There is no deadline-disable setting. The 24-hour fallback is user-chosen policy, not a workload-calibrated threshold. New root admissions, including macros and incoming MCP requests, always receive finite deadlines; replay and config reload retain existing frozen records.

#### Runtime-Owned Deadlines & Worker Enforcement

Autonomous run deadlines belong to the runtime, not the delegating model:

- **Root run boundary**: A trusted external admission (such as an interactive user command, a top-level CLI prompt, or an HTTP request via `harnx-serve`) mints an immutable `RunIdentity` and sets the initial run deadline. HTTP callers can only shorten this deadline at admission; they cannot extend it beyond configured policy.
- **Inheritance across delegations**: All child delegations, handoffs, and resumed sessions within that autonomous run inherit the root run's deadline (`parent.deadline`). Autonomous activity cannot renew or extend this deadline.
- **Worker-owned enforcement**: The worker daemon enforces deadlines independently while awaiting models, tool executions, retries, and nested sub-agents. If a calling process disconnects or dies while waiting, the detached worker continues running under its independent deadline and terminates cleanly when that deadline expires.

#### Result Envelope on Limit Reached

When a sub-agent invocation reaches a timeout or repetition limit:
1. The child session turn is cancelled through the worker cancellation path.
2. The sub-agent tool completes as a normal `Ok` tool result (not an unhandled tool error).
3. The returned JSON object includes a synthesized explanation in `response`, terminal progress in `sub_agent_progress`, and structured metadata in `termination`:

```json
{
  "session_id": "01948a3f-7b1c-7123-8901-abcdef123456",
  "response": "The invocation was stopped after reaching its time limit.\n\nPublic progress (partial):\nAnalysis complete for auth middleware. Drafted fix in plan.\nReferences: cid:plan:pantheon%2Fatlas/armDRA/issue-2222\n\nNo thinking text was captured (the non-streaming path produces none mid-call).\n\nLocal invocation allowance expired. Inspect saved public results and revise or narrow instructions before continuing the same session id `01948a3f-7b1c-7123-8901-abcdef123456`, only while the outer run remains live. Do not retry unchanged.\n\nUsage: used 165 budgeted tokens.",
  "sub_agent": {
    "agent": "researcher",
    "session_id": "01948a3f-7b1c-7123-8901-abcdef123456"
  },
  "sub_agent_progress": {
    "invocation_id": "8aa9a68a-034e-4df3-a9cf-6db978644f30",
    "agent": "researcher",
    "session_id": "01948a3f-7b1c-7123-8901-abcdef123456",
    "status": "failed",
    "elapsed_ms": 30012,
    "usage": {
      "input_tokens": 120,
      "output_tokens": 45,
      "cached_tokens": 0
    },
    "tool_call_count": 1
  },
  "termination": {
    "kind": "timeout",
    "session_id": "01948a3f-7b1c-7123-8901-abcdef123456",
    "usage": {
      "input_uncached": 120,
      "cache_write": 0,
      "output": 45,
      "budgeted": 165
    },
    "thinking_excerpt": null,
    "retry_hint": "Local invocation allowance expired. Inspect saved public results and revise or narrow instructions before continuing the same session id `01948a3f-7b1c-7123-8901-abcdef123456`, only while the outer run remains live. Do not retry unchanged.",
    "scope": "local_invocation",
    "deadline": "2026-10-02T05:30:00Z",
    "run_id": "01948a3f-7b1c-7000-8000-000000000001",
    "invocation_id": "8aa9a68a-034e-4df3-a9cf-6db978644f30",
    "public_progress": {
      "available": true,
      "output_excerpt": "Analysis complete for auth middleware. Drafted fix in plan.",
      "references": [
        "cid:plan:pantheon%2Fatlas/armDRA/issue-2222"
      ]
    }
  }
}
```

Field reference for `termination`:
- `kind`: `"timeout"` or `"repetition"` (or `"budget_exceeded"` in CLI contexts).
- `session_id`: Session ID of the stopped turn.
- `usage`: Token metrics for the turn (`input_uncached`, `cache_write`, `output`, `budgeted`).
- `scope`: Timeout scope derived from the worker's frozen record:
  - `"local_invocation"`: Only the local invocation allowance expired while the outer run deadline remains live.
  - `"inherited_deadline"`: An ancestor run deadline expired.
  - `"outer_run"`: The top-level run deadline expired.
- `deadline`: Absolute UTC ISO-8601 timestamp of the frozen deadline.
- `run_id`: UUID of the root run.
- `invocation_id`: UUID of this specific invocation.
- `public_progress`: Available partial output and artifact references:
  - `available`: Boolean indicating whether public output or artifact references were captured.
  - `output_excerpt`: Bounded public assistant text (up to 4 KiB).
  - `references`: Array of up to 16 canonical `cid:` URLs discovered during execution (up to 1,024 bytes per URL).
  - If no output or artifact references were captured, `available` is `false` and public progress is reported as unavailable.
- `thinking_excerpt`: Bounded tail of thinking text (up to 4 KiB) when streaming is active, or `null` if none was captured (non-streaming requests yield no intermediate thinking text mid-call).
- `retry_hint`: Guidance describing continuation rules based on termination scope.
- For `"repetition"` stops: `source` (`"tool_calls"`, `"answer"`, or `"thinking"`), and for tool calls `tool` and `count`.

#### Tool-Local Continuation Advice

The tool's `response` and `termination.retry_hint` explain each stop. An expired inherited or outer deadline says not to retry and to return to the user to confirm continuation with a new external instruction. A local timeout allows revised instructions to the same `session_id` only while the ancestor run remains live; local scope alone doesn't guarantee it is still live when the reply arrives. Repetition stops ask for a changed approach. Available durable public output and artifact references accompany the advice; missing output is reported as unavailable.

Expired admission or pre-dispatch failures also identify the exhausted scope and explain continuation, even when returned through an error envelope rather than a completed worker result. They don't claim that new tool output was produced. These are tool-local responses, not policies to copy into every agent prompt.

#### System Boundaries and Operational Invariants

- **Trusted frontend boundary**: Trusted external admissions establish run boundaries. Mid-turn user inputs steer an active turn without resetting or extending the autonomous run deadline.
- **Persisted run records & replay recovery**: Admission records (`RunLimitsRecord`) are persisted durably before execution. On replay or worker restart, the saved record is loaded rather than computing `now + timeout`. Completed child results are recovered before checking expiry, ensuring successful historical work is not converted into a false timeout.
- **Every prompt needs an admission**: A worker runs only a prompt with a durable admission. One without, such as a prompt sent before run admissions existed, ends its session's turn with an error asking for it to be sent again, which admits it as a new run. It never runs under default limits.
- **Cancellation is not rollback**: Terminating an invocation interrupts pending turns, but does not revert side effects already executed (such as files created or edited, git commits, or external tool executions).
- **Bounded clock skew**: Distributed nodes assume bounded clock skew across machines for UTC deadline evaluation.
- **Tool metadata snapshots**: Tool descriptions advertise target policy resolved at worker registration. Modifying agent front-matter or package patches after registration requires re-registering or restarting the worker to advertise updated descriptions, though worker admissions resolve against current files on disk.

#### Current-Run Timing Notices

The runtime delivers timing notices when due at safe boundaries before model calls. These are informative messages about the current run's elapsed time and deadline progress, not executable prompts. They don't advance the turn cursor, require admissions, or create new invocations.

The clock measures the current run, not the conversation's lifetime. Admitted runs start at the saved admission timestamp, including setup time before execution. A child run uses its own admission timestamp and its effective deadline, clamped to its ancestor's deadline. Retries and worker recovery don't reset or extend these saved limits.

**What gets delivered:**
- **Initial notice** (after the first user prompt in a run): The run's start timestamp, current timestamp, elapsed time, and effective deadline, remaining time, and total budget when a deadline exists.
- **Deadline warnings** (at 50%, 75%, 90% of the deadline budget): Updated timing context, including elapsed time, remaining time, and total budget.
- **Elapsed updates** (every 15 minutes of run time): The total elapsed time, even when there's no deadline.

**Delivery behavior:**
- Timing notices are delivered at the **safe boundary** between tool result return and the next model call. Long-running tools may delay these notices — they don't interrupt in-progress work or create additional model calls.
- Notices are coalesced: if multiple milestones are reached before a safe boundary, the notice covers all of them in a single receipt.
- Unmanaged runs without a deadline receive initial and elapsed-time notices, not deadline warnings. Admitted roots still have a finite deadline, including the default 24-hour fallback.
- The initial notice repeats on every new run. A new root admission (external prompt), child invocation, or handoff invocation starts a fresh timing context; retries within the same invocation don't re-emit already-covered milestones.
- Timing notices are persisted in the session log as `[Runtime note]` messages with a reserved identity, even when ordinary user-input appends are skipped (`skip_user_log_append`). Stable append IDs prevent retry duplicates. Recovery reads receipts from the raw log, so compaction can archive old timing context without losing milestone progress.

**What this doesn't guarantee:** Timing notices inform the model about remaining budget, but they don't guarantee the model can finish before the deadline. Long tools may still run beyond the boundary where a warning would be delivered.

**Deployment compatibility:** Pre-receipt workers treat durable timing rows as executable prompts and can refuse activation for missing admission. Writers are always on, so use the [hard-cutover and retained-reader rollback procedure](kubernetes-deployment.md#timing-receipt-compatibility-cutover-and-rollback). Draining sessions alone does not permit an old-reader rollback; completed histories still contain receipts.

### Live Event Streaming for User Interfaces

Because sub-agents run as standard NATS agent sessions, their execution events (LLM streaming chunks, tool invocations, notices) publish in real time to NATS:

```
sessions.{session_id}.events
```

Client interfaces can attach to a child session's stream to render activity
live. The TUI inserts a compact selectable child row with an animated running
spinner, locally advancing elapsed time, separate input/output/cached-token
counts, and tool-call count. It freezes elapsed time at completion and opens
the child's full transcript when the row receives focus. Nested rows can be
used to drill into grandchildren; press `Esc` to return one level. The Web UI
renders the same metrics under the parent assistant message, restores completed
rows from session history, and retains navigation into the child session.
Child output remains in the child transcript rather than being rendered inline
in the parent.

Interfaces learn about a delegation from `TurnEvent::SubAgentProgress` events on
the parent session's stream (`sessions.{parent_session_id}.events`). The first
is a running snapshot published as soon as the child session is bound. More
follow whenever tokens or the tool count change, every 10 seconds as a
heartbeat, and once more with `done` or `failed` status at termination. These
snapshots carry the same fields as the durable `sub_agent_progress` result
above, plus `tool_call_id`, the parent transcript's id for the call, so an
interface that reloaded the parent can place a running child under the message
that started it. Raw child transcript events are not forwarded to the parent
stream.

### Passing Attachments to Sub-Agents

When delegating tasks using `{agent}_session_prompt`, callers can pass an optional `attachments` array of canonical `cid:` URLs (`cid:media:...` or `cid:plan:...`):

```json
{
  "message": "Review the plan and investigate the attached profile",
  "attachments": [
    "cid:media:pantheon%2Fatlas/armDRA/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "cid:plan:pantheon%2Fatlas/armDRA/memory-leak-fix"
  ]
}
```

Harnx validates each URL before launching the sub-agent turn and drops duplicates. Image attachments resolve to native image content parts for the model. For text media and plan documents, Harnx appends pointer lines (`\nAttachment: <url>`) to the prompt text so the sub-agent can inspect them with `attachment_read`.

### Returning Files with Canonical cid: URLs

Agents and sub-agents should return canonical `cid:` URLs when producing files, reports, or plans:

1. **Create media attachments**: Call `attachment_create(content, mime_type)` to store text payloads (markdown summaries, logs, JSON reports) in NATS Object Storage (`harnx_attachments`). The tool returns a canonical `cid:media:<agent>/<session-id>/<hash>` URL.
2. **Create or update plans**: Call `plans_add_plan`, `plans_add_task`, or `plans_add_note` to record structured planning items. The tools return and accept canonical `cid:plan:<agent>/<session-id>/<slug>` URLs.
3. **Return the URL**: Return the canonical `cid:` URL in the final response. Parent agents, sub-agents, or users can read the content with `attachment_read`, inspect it with `harnx dump attachment <url>`, or open it in the default system viewer with `harnx open attachment <url>`.

## Examples

### Simple Assistant

A minimal agent at `<config-dir>/agents/grammar-genie.md`:

```markdown
---
model: openai:gpt-4o
temperature: 0
---
Your task is to take the text provided and rewrite it into a clear,
grammatically correct version while preserving the original meaning
as closely as possible. Correct any spelling mistakes, punctuation errors,
verb tense issues, word choice problems, and other grammatical mistakes.
```

### Code Assistant with Tools

An agent with access to filesystem and shell tools:

```markdown
---
model: claude:claude-3-5-sonnet
use_tools:
  - fs_*
  - bash_exec
description: Coding assistant with file and shell access
---
You are an expert software engineer. You can read and write files,
and run shell commands to help the user with coding tasks.

The user is working on {{__os__}} ({{__arch__}}) with {{__shell__}}.
Their current directory is {{__cwd__}}.
```

### Agent with File-sourced Variables

An agent that loads project conventions from a file:

```markdown
---
variables:
  - name: project
    description: Project name
    default: my-project
  - name: conventions
    description: Coding conventions
    path: code-assistant/conventions.md
---
You are a coding assistant for the {{project}} project.

Follow these conventions:
{{conventions}}
```

The file `<config-dir>/agents/code-assistant/conventions.md` is read at startup and its content replaces `{{conventions}}` in the prompt.

### Agent with Documents

An agent that uses RAG to answer questions from project docs:

```markdown
---
model: openai:gpt-4o
documents:
  - project-docs/architecture.md
  - project-docs/api-reference.md
  - project-docs/changelog.md
description: Project documentation assistant
---
You are a project assistant. Answer questions using the provided
documentation. If the docs don't cover something, say so clearly.
```


### Dynamic Variables in Prompts

Agent prompts are rendered using [MiniJinja](https://github.com/mitsuhiko/minijinja). In addition to the custom variables defined in front-matter, several dynamic variables are available:

- `{{ agent.model }}`: The active model ID (e.g., `openai:gpt-4o`). This updates automatically if the agent falls back to a different model.
- `{{ tools }}`: A list of available tools. You can iterate over them: `{% for t in tools %}- {{ t.name }}: {{ t.description }}{% endfor %}`.
- `{{ __os__ }}`, `{{ __arch__ }}`, `{{ __shell__ }}`, `{{ __cwd__ }}`, `{{ __now__ }}`, `{{ __locale__ }}`: Environment and system information.

### Detecting Environment-Specific Tooling

Sometimes you need agent behavior to adapt based on which tools are available. For example, agents running in a Kubernetes sandbox have different filesystem and workflow constraints than agents running locally.

**Check `tools`, not `agent.use_tools`.** The `tools` variable contains resolved tool declarations—only tools that are actually registered and available. The `agent.use_tools` field lists declared tool names from front-matter, which may include tools that don't exist in the current environment. Checking `agent.use_tools` produces false positives when tools are declared but not registered.

**Detection idiom.** To check whether a specific tool is available:

```jinja
{% set has_sandbox = (tools | selectattr('name', 'equalto', 'sandbox_connect') | list | length) > 0 %}
{% if has_sandbox %}
{{sandbox_workflow}}
{% else %}
<local workflow instructions>
{% endif %}
```

**Single-pass rendering constraint.** File-backed variables (those with `path:` in front-matter) are substituted as literal strings without MiniJinja re-evaluation. Template tags (`{% if %}`, `{{ var }}`) inside fragment files are emitted verbatim, not evaluated. If you need conditional branching, place the `{% if %}` block in the agent body and keep fragments as pure Markdown.

**Workflow categories.** Pantheon agents use three patterns for sandbox-adaptive prompts:

| Category | When to use | Workflow fragment |
|----------|-------------|-------------------|
| **CREATE** | Orchestrators that create and own sandbox lifecycle | `shared/sandbox-workflow-create.md` |
| **INHERIT** | Workers delegated by an orchestrator (sandbox already bound) | `shared/sandbox-workflow-inherit.md` |
| **DUAL** | Research agents that may run standalone or delegated | `shared/sandbox-workflow-dual.md` |
