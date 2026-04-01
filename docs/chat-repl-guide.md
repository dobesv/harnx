# Chat REPL Guide

The core of Harnx is Chat-REPL.

## REPL Features

- **Tab Autocompletion:** All REPL commands have completions.
  - `.<tab>` to complete REPL commands.
  - `.model <tab>` to complete chat models.
  - `.set <tab>` to complete config keys.
  - `.set key <tab>` to complete config values.
- **Multi-line Support:** Input multi-line text in the following ways:
  - Press `ctrl+o` to edit buffer with an external editor (recommend).
  - Paste multi-line text (requires terminal support for bracketed paste).
  - Type `:::` to start multi-line editing, type `:::` to finish it.
  - Use hotkey `{ctrl,shift,alt}+enter` or `ctrl+j` to insert a newline directly.
- **History Search:** Press `ctrl+r` to search the history. Use `↑↓` to navigate through the history.
- **Configurable Keybinding:** Emacs-style bindings and basic VI-style.
- **[Custom REPL Prompt](custom-repl-prompt.md):** Display information about the current context in the prompt.

## REPL Commands

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

### `.role` - role management

```
.role                    Create or switch to a role
.info role               Show role info
.edit role               Modify current role
.save role               Save current role to file
.exit role               Exit active role
```

### `.prompt` - set a temporary role using a prompt

Compared to `.role`, `.prompt` does not persist to a file; it creates and switches to a temporary role.

### `.session` - session management

```
.session                 Start or switch to a session
.empty session           Clear session messages
.compress session        Compress session messages
.info session            Show session info
.edit session            Modify current session
.save session            Save current session to file
.exit session            Exit active session
```

### `.agent` - chat with AI agent

```
.agent                   Use an agent
.starter                 Use a conversation starter
.edit agent-config       Modify agent configuration file
.info agent              Show agent info
.exit agent              Leave agent
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

> NOTE: `%%` and `cmd` are supported starting from V0.27.0.

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

### `.edit` - modify config/role/session/agent-config/rag-docs

```
.edit config             Modify configuration file
.edit role               Modify current role
.edit session            Modify current session
.edit agent-config       Modify agent configuration file
.edit rag-docs           Add or remove documents from an existing RAG
```

### `.delete` - delete roles/sessions/RAGs/agents

### `.info` - display system/role/session/agent/RAG info

```
.info                    Show system info
.info role               Show role info
.info session            Show session info
.info agent              Show agent info
.info rag                Show RAG info
```

### `.exit` - exit role/session/RAG/agent/REPL

```
.exit role               Exit active role
.exit session            Exit active session
.exit agent              Leave agent
.exit rag                Leave RAG
.exit                    Exit REPL
```

### `.help` - show help guide
