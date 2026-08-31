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
- **Multi-line input** via paste (bracketed-paste terminals) or `Shift+Enter` / `Ctrl+J` to insert a newline.
- **History:** `↑` / `↓` to navigate prior submissions.
- **Attachments:** `.attach <path>` to attach a file to the next message; `.detach` to remove.
  - **Large pastes** (more than 8 lines or more than 512 characters) are automatically saved as a text attachment instead of being inserted inline. Smaller multi-line pastes are inserted directly into the input.
- **Sub-agent monitoring:** Delegated child sessions appear as compact rows with
  running, done, or failed status, elapsed time, input/output/cached tokens, and
  tool-call count. Running rows use the normal animated spinner and advance the
  elapsed time locally between progress snapshots; terminal rows freeze at the
  completed duration. Focus a row with normal transcript `↑` / `↓` navigation
  and press `Enter` to open the child's complete transcript fullscreen.

### Transcript and sub-agent navigation

When the input is blank, `↑` enters transcript navigation. Navigable rows
include user and assistant messages, tool calls, compaction markers, and
sub-agent sessions. Press `Enter` on a focused sub-agent row to open it in the
fullscreen transcript surface.

Inside a child transcript:

- `↑` / `↓` moves between navigable rows and keeps the focused row visible.
- `Enter` opens full details for a regular row or drills into a focused nested
  sub-agent session.
- `PgUp` / `PgDn` or mouse-wheel scrolling moves through the displayed child.
- `Esc` returns to the parent transcript one level at a time.

Child activity is monitored separately from the root turn, so it does not
change the root input, busy indicator, or streaming message. Completed child
views remain available while the same root session is selected; switching the
root agent/session closes all child monitors and clears those views.
Each delegation has its own row and progress counters, so prompting an existing
child session again does not overwrite the earlier invocation's summary.

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
.info session [<agent> <id>] Show session state in overlay
.edit session            Modify current session
.save session            Save current session to file
```

### `.agent` - agent management

```
.agent                   Switch to an agent
.info agent [<name>]     Show rendered agent in overlay
.edit agent              Edit agent .md file
.save agent [name]       Save current agent to file
.starter                 Use a conversation starter
```

### `.rag` - chat with documents

```
.rag                     Initialize or access RAG
.edit rag-docs           Add or remove documents from an existing RAG
.rebuild rag             Rebuild RAG for document changes
.sources rag             Show citation sources used in last query
.info rag                Show RAG info (in transcript)
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
.set compress_threshold 1000
.set rag_reranker_model <tab>
.set rag_top_k 4
.set highlight true
.set title My Custom Session Title
```

`.set title <text>` sets a title for the current session. The text may contain
spaces. Setting a title manually freezes automatic title regeneration for the
rest of the session. See the [Configuration Guide](configuration-guide.md#session-titles)
for automatic title generation (`title_agent`, `title_update_threshold`).

### `.edit` - modify config/session/agent/rag-docs

```
.edit config             Modify configuration file
.edit session            Modify current session
.edit agent              Edit agent .md file
.edit rag-docs           Add or remove documents from an existing RAG
```

### `.delete` - delete agents/sessions/RAGs

### `.info` - display system/session/agent/RAG info

The `.info agent` and `.info session` commands display detailed information in a fullscreen scrollable overlay. Press `Esc` to close, and use arrow keys or `PgUp`/`PgDn` to scroll.

- **`.info agent [<name>]`**: Shows the fully-rendered agent configuration (patches applied, prompt interpolated, and tools expanded).
  - If `<name>` is omitted, it defaults to the active agent.
  - **Note:** This replaces the old raw source view. To view the raw agent file, use `cat ~/.config/harnx/agents/<name>.md`.
  - During expansion, if an MCP server fails, a warning is logged to stderr and the process continues with remaining tools.
- **`.info session [<agent> <id>]`**: Shows the session state (history, tokens, variables, etc.).
  - If arguments are omitted and a session is active, it shows the active session.
  - Does **not** include the system prompt and does **not** launch MCP servers.
- **`.info`**, **`.info rag`**, **`.info tools`**: These commands continue to append information directly to the chat transcript.

### `.exit` - exit the current scope

```text
.exit rag                Leave RAG
.exit                    Exit the interactive session
```

### `.help` - show help guide
