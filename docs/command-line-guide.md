# Command Line Guide

## Usage

```
Usage: harnx [OPTIONS] [COMMAND]

Commands:
  prompt  Run a non-interactive prompt
  info    Inspect harnx state
  session Manage sessions

Options:
  -m, --model <MODEL>                  Select a LLM model
      --prompt <PROMPT>                Use the system prompt
  -s, --session [<SESSION>]            Start or join a session
      --empty-session                  Ensure the session is empty
      --save-session                   Ensure the new conversation is saved to the session
  -a, --agent <AGENT>                  Start an agent
      --agent-variable <NAME> <VALUE>  Set agent variables
      --rag <RAG>                      Start a RAG
      --rebuild-rag                    Rebuild the RAG to sync document changes
      --macro <MACRO>                  Execute a macro
  -t, --tool <TOOL>                    Use specific tools
  -f, --file <FILE>                    Include files, directories, or URLs
  -S, --no-stream                      Turn off stream mode
      --final-only                     Print only the final response
      --dry-run                        Display the message without sending it
      --info                           Display information
      --sync-models                    Sync models updates
      --list-models                    List all available chat models
      --list-sessions                  List all sessions
      --list-agents                    List all agents
      --list-rags                      List all RAGs
      --list-macros                    List all macros
  -h, --help                           Print help
  -V, --version                        Print version
```

## Examples

```
harnx                                          # Enter the interactive TUI
harnx prompt Tell a joke                       # Generate response
harnx -- Tell a joke                           # Same, using an explicit separator

harnx-serve                                    # Run server (standalone binary)
harnx-serve --addr 0.0.0.0:8080                # Run server with addr

harnx -m openai:gpt-4o                         # Select LLM

harnx -s                                       # Begin a temp session
harnx -s session1                              # Use session 'session1'
harnx -a agent1                                # Use agent 'agent1'
harnx --rag rag1                               # Use RAG 'rag1'

harnx info agent agent1                        # View agent info
harnx info session agent1 session1             # View session info
harnx --info                                   # View system info
harnx --rag rag1 --info                        # View RAG info

harnx --macro macro1                           # Execute macro 'macro1'
harnx --macro macro2 -- arg1 arg2              # Execute macro 'macro2' with args

output=$(harnx prompt --final-only -- "$input") # Return only the final response
cat prompt.txt | harnx prompt                    # Read the prompt from stdin

harnx prompt -f a.png -f b.png diff images     # Use files
```

Free-form command-line text must be introduced by the `prompt` subcommand or
an explicit `--` separator. This keeps prompts such as `info` and `session`
distinct from CLI subcommands. Use `prompt` as the canonical syntax for piped
stdin and file-only input as well. Bare stdin and file-only forms remain
supported for compatibility.

For a normal one-shot prompt, Harnx writes the root agent and session heading
to stderr after creating the session. Delegations also write a start line, a
running status line every 10 seconds, and an immediate done or failed line.
Each status identifies the child agent/session and reports elapsed time,
input/output/cached tokens, and tool calls; concurrent children are tracked
independently.

`--final-only` suppresses the root heading, delegation status, and all other
startup/progress output. On success, stdout contains only the final response,
which makes the mode safe for command substitution and pipelines.

## Shell Integration

Simply type `alt+e` to let `harnx` provide intelligent completions directly in your terminal.

Harnx offers shell integration scripts for bash, zsh, PowerShell, fish, and nushell. You can find them on GitHub at [https://github.com/dobesv/harnx/tree/main/scripts/shell-integration](https://github.com/dobesv/harnx/tree/main/scripts/shell-integration).

## Shell Autocompletion

The shell autocompletion suggests commands, options, and filenames as you type, enabling you to type less, work faster, and avoid typos.

Harnx offers shell completion scripts for bash, zsh, PowerShell, fish, and nushell. You can find them on GitHub at [https://github.com/dobesv/harnx/tree/main/scripts/completions](https://github.com/dobesv/harnx/tree/main/scripts/completions).

## Use Files & URLs

The `-f/--file` flag can be used to send files to LLMs.

```
# Use local file
harnx prompt -f data.txt
# Use image file
harnx prompt -f image.png ocr
# Use multiple files
harnx prompt -f file1 -f file2 explain
# Use local dirs
harnx prompt -f dir/ summarize
# Use remote URLs
harnx prompt -f https://example.com/page summarize
```

## Run Server

Use standalone `harnx-serve` binary for HTTP server mode.

```sh
harnx-serve --addr 127.0.0.1:8000
Embeddings API:       http://127.0.0.1:8000/v1/embeddings
Rerank API:           http://127.0.0.1:8000/v1/rerank
```

Common flags:

```sh
harnx-serve --addr 0.0.0.0:8000
harnx-serve --model claude:claude-3-5-sonnet-20240620
harnx-serve --dry-run
harnx-serve --agent-variable env production --agent-variable debug true
```

## Inspect Agents and Sessions

Use the `info` subcommand to inspect the state of agents and sessions.

### `harnx info agent <name>`

Prints the fully-rendered agent configuration to stdout. This includes:
- YAML front-matter with package patches applied and `use_tools` wildcards expanded to concrete tool names via live MCP servers.
- The system prompt with all variables and templates (MiniJinja) interpolated.

If an MCP server fails during tool expansion, a warning is logged to stderr, and the command continues with the remaining tools.

### `harnx info session <agent-name> <session-id>`

Prints the session state (model, tokens, variables, history, and snapshots) to stdout.
- This command does **not** include the system prompt.
- It does **not** launch MCP servers.
