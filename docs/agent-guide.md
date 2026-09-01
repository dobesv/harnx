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
- Passing a session owned by another agent fails before the prompt is appended.

Do not invent an ID when a generated session is desired. The committed target
ID is reported only after the prompt is persisted and worker activation is
published. Bare and package-qualified targets stay on the current cluster;
explicit `agent@cluster` targets use that configured cluster.

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
  - `{agent}_session_prompt`: Sends a prompt message (`message`, optional `session_id`, optional `timeout_secs`, optional `token_budget`) to a sub-agent session, returning the sub-agent's final response text or a synthesized termination result. The parent session ID is propagated internally.
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

### Per-Invocation Execution Limits & Sub-Agent Termination

Parent agents can pass per-invocation execution limits when calling `{agent}_session_prompt`:

- `timeout_secs` (integer, seconds): Maximum time allowed for the sub-agent invocation. Passing `0` or omitting the argument sets no time limit.
- `token_budget` (integer, tokens): Maximum cumulative tokens allowed for the sub-agent invocation. Passing `0` or omitting the argument sets no token limit.

Interactive user paths (TUI and Web UI) are unbounded by design.

#### Result Envelope on Limit Reached

When a sub-agent invocation reaches a timeout or token budget limit:
1. The child session turn is hard-cancelled through the background cancellation path.
2. The sub-agent tool completes as a normal `Ok` tool result (not a tool execution error).
3. The returned JSON object includes a synthesized explanation in `response`, plus a structured `termination` sub-object:

```json
{
  "session_id": "01948a3f-7b1c-7123-8901-abcdef123456",
  "response": "The invocation was stopped after reaching its time limit.\n\nNo thinking text was captured (the non-streaming path produces none mid-call).\n\nYou can retry by sending a new message to the same session id `01948a3f-7b1c-7123-8901-abcdef123456` with revised or narrower instructions.\n\nUsage: used 165 budgeted tokens.",
  "sub_agent": {
    "agent": "researcher",
    "session_id": "01948a3f-7b1c-7123-8901-abcdef123456"
  },
  "sub_agent_progress": {
    "invocation_id": "8aa9a68a-034e-4df3-a9cf-6db978644f30",
    "agent": "researcher",
    "session_id": "01948a3f-7b1c-7123-8901-abcdef123456",
    "status": "done",
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
    "retry_hint": "You can retry by sending a new message to the same session id `01948a3f-7b1c-7123-8901-abcdef123456` with revised or narrower instructions."
  }
}
```

The parent agent can inspect `termination.kind` (`"timeout"` or `"budget_exceeded"`) and retry by sending a new message to the same `session_id`.

#### Design Guarantees & Limitations

- **Budget metric & scope**: Token budget metric is `(input_tokens - cached_tokens) + output_tokens` (uncached input + cache writes + output; cache reads excluded). It applies per invocation as a fresh delta; retrying a session starts a clean token budget allowance. Workers evaluate budget before each model call, so at least one model call executes when `token_budget > 0`.
- **Enforcement asymmetry**: `timeout_secs` is enforced on the caller side and stops invocations whose calling session remains alive. If a calling process dies while waiting, caller-side timeout does not stop the detached sub-agent worker. However, `token_budget` is enforced worker-side before model calls, bounding cost even for orphaned workers.
- **Thinking excerpts**: Non-streaming model calls do not yield intermediate thinking text during an active request, so mid-call timeouts produce an empty thinking excerpt ("none captured"). Budget limits trigger at turn boundaries and can capture thinking excerpts when streaming is enabled.

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

To allow interfaces to attach before the sub-agent prompt completes, an early advisory event is emitted on the parent session's stream (`sessions.{parent_session_id}.events`) immediately when delegation begins:

```rust
AgentEvent::SubAgent {
    source: AgentSource { agent: "researcher", session_id: Some("01948..."), model: None },
    event: Box::new(AgentEvent::Turn(TurnEvent::SubAgentStarted {
        agent: "researcher",
        session_id: "01948...",
        invocation_id: Some("8aa9a68a-034e-4df3-a9cf-6db978644f30"),
    })),
}
```

`invocation_id` is optional on `SubAgentStarted` for compatibility with older
producers. New producers follow it with `TurnEvent::SubAgentProgress` events on
the parent stream. A running snapshot is published whenever tokens or the tool
count changes, every 10 seconds as a heartbeat, and once more with `done` or
`failed` status at termination. These snapshots carry the same fields as the
durable `sub_agent_progress` result above. Raw child transcript events are not
forwarded to the parent stream.

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
