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
  - fs:all
  - bash_exec
description: A helpful coding assistant
version: "1.0"
agent_default_session: default
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
| `temperature` | `float` | global default | Controls randomness (0 = deterministic, 1 = creative). Inherited from global config when `model` is omitted. |
| `top_p` | `float` | global default | Nucleus sampling parameter. Alternative to temperature. Inherited from global config when `model` is omitted. |
| `use_tools` | `list` | none | YAML list of tool specifiers. Also accepts a comma-separated string for backward compatibility. See [Tools](#tools). |
| `description` | `string` | `""` | Short description shown in agent listings. |
| `version` | `string` | `""` | Version identifier for the agent. |
| `variables` | `list` | `[]` | Variables prompted on first use. See [Variables](#variables). |
| `conversation_starters` | `list` | `[]` | Suggested prompts shown when starting the agent in REPL mode. |
| `documents` | `list` | `[]` | Document paths for RAG integration. See [Documents](#documents-rag). |
| `agent_default_session` | `string` | none | Session to auto-load when starting this agent (e.g. `temp`, `default`). |
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

The `use_tools` field controls which MCP tools the agent can access. Tools are specified as a YAML list (a comma-separated string is also accepted for backward compatibility).

### Syntax

| Pattern | Meaning |
|---|---|
| `tool_name` | Enable a single tool by name |
| `server:all` | Enable all tools from an MCP server |
| `all` | Enable every available tool |
| `toolset_name` | Enable a named toolset (defined in global config) |

### Examples

```yaml
# Single tools
use_tools:
  - web_search
  - execute_command

# All tools from a server
use_tools:
  - fs:all
  - git:all

# Everything
use_tools:
  - all

# Mix of patterns
use_tools:
  - fs:all
  - web_search
  - my_toolset
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

Hooks let you run external commands at specific points during agent execution. They're configured under the `hooks` front-matter field.

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
| `type` | `string` | yes | — | Execution protocol. Supported: `claude-command`, `claude-command-persistent` |
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

### From the REPL

```
.agent <name>        Switch to an agent
.info agent          Show current agent info
.edit agent          Edit the agent's .md file
.save agent [name]   Save current agent configuration
.exit agent          Exit the active agent
```

### Inline Prompt

Use `--prompt` to create a temporary agent without a file:

```sh
harnx --prompt "You are a helpful translator" "translate hello to French"
```

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
  - fs:all
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

## Built-in Agents

Harnx includes one internal agent:

- `%create-title%`: Generates session titles automatically. This is internal and not intended for direct use.
