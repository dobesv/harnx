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
    - event: Stop
      type: claude-command
      command: "/path/to/hook.sh"
      timeout: 30
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

Running `harnx -a my-agent "What is Rust?"` produces these messages:

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
    - event: PreToolUse
      type: claude-command
      matcher: shell
      command: "/path/to/approve-tool.sh"
      timeout: 15
      async: false
    - event: Stop
      type: claude-command
      command: "/path/to/on-stop.sh"
      status_message: "Running stop hook..."
      async: true
```

### Hook Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `event` | `string` | yes | — | Hook event name (e.g. `PreToolUse`, `Stop`, `SessionStart`) |
| `type` | `string` | yes | — | Execution protocol. Supported: `claude-command`, `claude-command-persistent`. Note: `PreToolUse` hooks can return `hookSpecificOutput.toolInput` to mutate tool arguments; `PostToolUse` hooks can return `hookSpecificOutput.toolResponse` to mutate the response. |
| `matcher` | `string` | no | none | Regex pattern to match against the tool name (for tool-related events) |
| `command` | `string` | yes | — | Shell command to execute |
| `timeout` | `integer` | no | `30` | Timeout in seconds |
| `status_message` | `string` | no | none | Message to display while the hook runs |
| `async` | `boolean` | no | none | Whether to run the hook asynchronously |

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
harnx --agent <name> "your question"    # Start with input
harnx --list-agents                     # List available agents
```

You can also pass variable values directly:

```sh
harnx --agent coder --agent-variable language=rust "write a web server"
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
harnx --prompt "You are a helpful translator" "translate hello to French"
```

## Sub-Agents & Agent Delegation

Harnx supports agent delegation, allowing a parent agent to run nested sub-agents for specialized tasks (such as code analysis, research, or execution planning).

### NATS-Based Session Model

Sub-agents in Harnx execute as standard NATS agent sessions (`ThinClientSession`). ACP (Agent Client Protocol) and its stdio child process architecture have been removed.

- **Markdown-only agent definitions**: Agents are defined solely by Markdown files with YAML front-matter in `<config-dir>/agents/*.md` (or package agents). ACP server configuration (`acp_servers/*.yaml`) and ACP stdio child processes no longer exist.
- **Auto-registered toolsets**: For every configured agent, the worker daemon registers a NATS-backed 4-tool toolset:
  - `{agent}_session_new`: Creates a new sub-agent session and returns its initial response along with session metadata.
  - `{agent}_session_prompt`: Sends a prompt message (`message`, optional `session_id`, optional `parent_session_id`) to a sub-agent session, returning the sub-agent's final text response.
  - `{agent}_session_load`: Reads prior event history for an existing sub-agent session log.
  - `{agent}_session_cancel`: Cancels an in-flight prompt on a sub-agent session.
- **Worker-agnostic execution**: Sub-agent turns route via standard NATS JetStream WorkQueue subjects (`WORK_NOTIFY_<cluster>`) and acquire distributed KV locks (`harnx_leases`). Execution can run on any available worker in a cluster.
- **Timeout protection**: Sub-agent tool calls run synchronously from the parent agent's perspective. Turn execution is bounded by idle and operation timeouts (`HARNX_SUBAGENT_IDLE_TIMEOUT_SECS` defaulting to 300s, `HARNX_SUBAGENT_OPERATION_TIMEOUT_SECS` defaulting to 3600s).

### Sub-Agent Tool Result Marker

When `{agent}_session_new` or `{agent}_session_prompt` completes, the tool returns a JSON object containing the sub-agent response text and a structured identification marker (`sub_agent`):

```json
{
  "session_id": "01948a3f-7b1c-7123-8901-abcdef123456",
  "response": "Analysis complete. Here are the findings...",
  "sub_agent": {
    "agent": "researcher",
    "session_id": "01948a3f-7b1c-7123-8901-abcdef123456"
  }
}
```

The `sub_agent` marker carries the exact `AgentSource` structure (`agent`, `session_id`), giving client interfaces explicit sub-agent identity without requiring tool-name heuristics.

### Live Event Streaming for User Interfaces

Because sub-agents run as standard NATS agent sessions, their execution events (LLM streaming chunks, tool invocations, notices) publish in real time to NATS:

```
sessions.{session_id}.events
```

Client interfaces (TUI, web UI, CLI) can attach to a child session's stream to render activity live.

To allow interfaces to attach before the sub-agent prompt completes, an early advisory event is emitted on the parent session's stream (`sessions.{parent_session_id}.events`) immediately when delegation begins:

```rust
AgentEvent::SubAgent {
    source: AgentSource { agent: "researcher", session_id: Some("01948..."), model: None },
    event: Box::new(AgentEvent::Turn(TurnEvent::SubAgentStarted {
        agent: "researcher",
        session_id: "01948...",
    })),
}
```

When a UI receives `TurnEvent::SubAgentStarted` on the parent stream, it can immediately subscribe to `sessions.{child_session_id}.events` for real-time progress before `{agent}_session_prompt` returns its final output.

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

