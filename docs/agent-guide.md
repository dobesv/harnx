# Agent Guide

## What is an Agent?

Agents customize LLM behavior by combining instructions (a system prompt) with model configuration, tools, variables, documents, and more. They're the core building block for tailoring Harnx to your workflow.

An agent is a Markdown file with YAML front-matter, stored at `<harnx-config-dir>/agents/<name>.md`. The front-matter holds configuration; the Markdown body provides the instructions (system prompt).

## Agent File Format

Here's a complete example showing all available front-matter fields:

```markdown
---
model: openai:gpt-4o             # LLM to use
temperature: 0                   # Creativity (0 = deterministic, 1 = creative)
top_p: null                      # Alternative diversity control
use_tools: null                  # MCP tools (e.g. 'fs:all,bash_exec')
description: ""                  # Short description
version: ""                      # Version string
agent_default_session: null      # Session to load when starting this agent
instructions: null               # Override the Markdown body below

variables:                       # Prompted on first use
  - name: project_dir
    description: The project directory
    default: "."

conversation_starters:           # Suggested starting prompts
  - What can you help me with?
  - Let's debug this issue

documents:                       # RAG document paths
  - docs/guide.md

hooks:                           # Per-agent hooks
  max_resume: 3
  entries:
  - event: Stop
    type: claude-command
    command: "/path/to/hook.sh"
---

You are a helpful AI assistant. Your task is to help the user with their questions and tasks.
```

### Front-matter Fields

| Field | Type | Description |
|---|---|---|
| `model` | string | LLM model ID (e.g. `openai:gpt-4o`, `claude:claude-3-5-sonnet`) |
| `temperature` | float | Controls randomness (0..1). Lower = more deterministic. |
| `top_p` | float | Nucleus sampling parameter. Alternative to temperature. |
| `use_tools` | string | Comma-separated MCP tool specs (e.g. `fs:all,bash_exec`) |
| `description` | string | Short description shown in agent listings |
| `version` | string | Version identifier |
| `agent_default_session` | string | Session to auto-load (e.g. `temp`, `default`) |
| `instructions` | string | Overrides the Markdown body as the system prompt |
| `variables` | list | Variables prompted on first use (see below) |
| `conversation_starters` | list | Suggested prompts shown when starting the agent |
| `documents` | list | Document paths for RAG integration |
| `hooks` | object | Per-agent hook configuration |

## Types of Prompts

There are three prompt patterns you can use in the Markdown body:

### Embedded Prompt

Contains `__INPUT__`, which gets replaced with your input. Good for concise, input-driven replies.

```markdown
---
---
convert __INPUT__ to emoji
```

Running `harnx -a emoji angry` generates:
```json
[
  {"role": "user", "content": "convert angry to emoji"}
]
```

### System Prompt

No `__INPUT__` placeholder. Sets general context for the LLM.

```markdown
---
---
convert my words to emoji
```

Running `harnx -a emoji angry` generates:
```json
[
  {"role": "system", "content": "convert my words to emoji"},
  {"role": "user", "content": "angry"}
]
```

### Few-shot Prompt

Uses `### INPUT:` and `### OUTPUT:` markers to provide example exchanges, giving the LLM more precise guidance.

````markdown
---
---
Provide only code without comments or explanations.
### INPUT:
async sleep in js
### OUTPUT:
```javascript
async function timeout(ms) {
  return new Promise(resolve => setTimeout(resolve, ms));
}
````

Running `harnx -a coder echo server in node.js` generates:
```json
[
  {"role": "system", "content": "Provide only code without comments or explanations."},
  {"role": "user", "content": "async sleep in js"},
  {"role": "assistant", "content": "```javascript\nasync function timeout(ms) {\n  return new Promise(resolve => setTimeout(resolve, ms));\n}\n```"},
  {"role": "user", "content": "echo server in node.js"}
]
```

## Examples

### Simple Agent (prompt only)

A minimal agent at `<harnx-config-dir>/agents/grammar-genie.md`:

```markdown
---
model: openai:gpt-4o
temperature: 0
---
Your task is to take the text provided and rewrite it into a clear, grammatically correct version while preserving the original meaning as closely as possible. Correct any spelling mistakes, punctuation errors, verb tense issues, word choice problems, and other grammatical mistakes.
```

### Agent with Tools

An agent that has access to specific MCP tools:

```markdown
---
use_tools: web_search,execute_command
---
```

### Agent with Variables

Variables let you create reusable agents with dynamic prompts:

```markdown
---
variables:
  - name: language
    description: Target programming language
    default: python
---
You are an expert {{language}} developer. Write clean, idiomatic {{language}} code.
```

When you start this agent, Harnx prompts you for the `language` value (or uses the default).

### Agent with Documents (RAG)

Attach documents for retrieval-augmented generation:

```markdown
---
documents:
  - project-docs/architecture.md
  - project-docs/api-reference.md
---
You are a project assistant. Answer questions using the provided documentation.
```

## Using Agents

### From the Command Line

```sh
harnx -a <name>                    # Start an agent
harnx -a <name> "your question"   # Start an agent with input
harnx --list-agents                # List available agents
```

### From the REPL

```
.agent <name>        Switch to an agent
.info agent          Show agent info
.edit agent          Edit agent .md file
.save agent [name]   Save current agent
.exit agent          Exit active agent
```

### Inline Prompt

Use `--prompt` to create a temporary agent without a file:

```sh
harnx --prompt "You are a helpful translator" "translate hello to French"
```

## Built-in Agents

Harnx includes one internal agent:

- `%create-title%`: Generates session titles automatically. This is internal and not intended for direct use.
