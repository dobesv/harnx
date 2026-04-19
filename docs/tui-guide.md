# TUI & Dot-Commands Guide

Harnx has two runtime modes:

- **CLI mode** – non-interactive, one-shot. Invoked when you pass a prompt, a file (`-f`), or STDIN. Runs the request and exits.
- **TUI mode** – interactive chat UI (ratatui-based). Entered by running `harnx` with no prompt or input. Inside the TUI you type messages or `.`-prefixed **dot-commands**.

There is no readline-based REPL.

## TUI Features

- **Tab Autocompletion** for dot-commands:
  - `.<tab>` to complete command names.
  - `.model <tab>` to complete chat models.
  - `.set <tab>` to complete config keys.
  - `.set key <tab>` to complete config values.
- **Multi-line input** via paste (bracketed-paste terminals) or `{ctrl,shift,alt}+enter` / `ctrl+j` to insert a newline.
- **History:** `↑` / `↓` to navigate prior submissions.
- **Attachments:** `.attach <path>` to attach a file to the next message; `.detach` to remove.

## Dot-Commands

### `.model` - change the current LLM

```
openai:gpt-4o     128000 /     4096  |       5 /     15    👁 ⚒
|                 |            |             |       |     |  └─ support function callings
|                 |            |             |       |     └─ support vision
|                 |            |             |       └─ output price ($/1M)
|                 |            |             └─ input price ($/1M)
|                 |            |
|                 |            └─ max output tokens
|                 └─ max input tokens
└─ model id
```

### `.prompt` - set a temporary agent using a prompt

`.prompt` creates a temporary agent from an inline prompt without persisting it to a file.

### `.session` - session management

```
.session                 Start or switch to a session
.empty session           Clear session messages
.compact session         Compact session messages using configured compaction agent
.info session            Show session info
.edit session            Modify current session
.save session            Save current session to file
.exit session            Exit active session
```

### `.agent` - agent management

```
.agent                   Switch to an agent
.info agent              Show agent info
.edit agent              Edit agent .md file
.save agent [name]       Save current agent to file
.exit agent              Exit active agent
.starter                 Use a conversation starter
```

### `.rag` - chat with documents

```
.rag                     Initialize or access RAG
.edit rag-docs           Add or remove documents from an existing RAG
.rebuild rag             Rebuild RAG for document changes
.sources rag             Show citation sources used in last query
.info rag                Show RAG info
.exit rag                Leave RAG
```

### `.macro` - execute a macro

```
.macro test-function-calling
.macro within-agent todo list all my todos
```

### `.file` - read files and use them as input

```
Usage: .file <file|dir|url|%%|cmd>... [-- <text>...]

.file data.txt
.file %% -- translate last reply to english
.file `git diff` -- generate git commit message
.file config.yaml -- convert to toml
.file screenshot.png -- design a web app based on the image
.file https://ibb.co/a.png https://ibb.co/b.png -- what is the difference?
.file https://github.com/dobesv/harnx/blob/main/README.md -- what are the features of Harnx?
```

### `.continue` - continue previous response

This command is often used to resume generation that was interrupted due to the response exceeding the length limit.

### `.regenerate` - regenerate the response

If the response is interrupted or unsatisfactory, you can regenerate it with `.regenerate`.

### `.copy` - copy last response

### `.set` - adjust runtime settings

```
.set <tab>
.set max_output_tokens 4096
.set temperature 1.2
.set top_p 0.8
.set dry_run true
.set stream false
.set save true
.set function_calling true
.set use_tools <tab>
.set save_session true
.set compress_threshold 1000
.set rag_reranker_model <tab>
.set rag_top_k 4
.set highlight true
```

### `.edit` - modify config/session/agent/rag-docs

```
.edit config             Modify configuration file
.edit session            Modify current session
.edit agent              Edit agent .md file
.edit rag-docs           Add or remove documents from an existing RAG
```

### `.delete` - delete agents/sessions/RAGs

### `.info` - display system/session/agent/RAG info

```
.info                    Show system info
.info session            Show session info
.info agent              Show agent info
.info rag                Show RAG info
.info tools              List available tools and their active state
```

### `.exit` - exit the current scope

```
.exit session            Exit active session
.exit agent              Exit active agent
.exit rag                Leave RAG
.exit                    Exit the interactive session
```

### `.help` - show help guide
