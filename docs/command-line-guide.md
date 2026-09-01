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
      --timeout-secs <SECONDS>         Set maximum invocation duration in seconds (0 or unset = no limit)
      --token-budget <TOKENS>          Set maximum budgeted tokens for invocation (0 or unset = unlimited)
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
harnx prompt --timeout-secs 30 --token-budget 100000 -- "Summarize system logs"
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

## Bounding a One-Shot Run

Non-interactive prompts (`harnx prompt` or `harnx -- <text>`) can set per-invocation execution limits using `--timeout-secs` and `--token-budget`. Interactive TUI sessions (`harnx`) and Web UI runs are unbounded by design.

- `--timeout-secs <SECONDS>`: Maximum execution time in seconds. Passing `0` or omitting the option means no time limit.
- `--token-budget <TOKENS>`: Maximum budgeted tokens for the invocation. Passing `0` or omitting the option means unlimited tokens.

### Limit Exhaustion Behavior

When a non-interactive invocation reaches either limit:
1. The active turn is hard-cancelled through the background cancellation path.
2. Harnx writes a synthesized human-readable explanation to `stdout`.
3. Harnx writes a single compact JSON line to `stderr`.
4. The process exits with code **2** (distinct from generic error exit code 1).

If `--final-only` is active, normal startup headers and progress lines are suppressed, but on limit exhaustion Harnx still prints the synthesized text to `stdout` and the JSON line to `stderr`.

### Stderr JSON Interface

The single stderr JSON line provides a stable, machine-readable contract for downstream scripts and tooling:

```json
{"kind":"timeout","session_id":"01948a3f-7b1c-7123-8901-abcdef123456","usage":{"input_uncached":120,"cache_write":0,"output":45,"budgeted":165},"thinking_excerpt":null,"retry_hint":"You can retry by sending a new message to the same session id `01948a3f-7b1c-7123-8901-abcdef123456` with revised or narrower instructions."}
```

Field reference:
- `kind`: `"timeout"` or `"budget_exceeded"`.
- `session_id`: Session ID of the cancelled turn. Pass this ID on a subsequent prompt command to retry in the same session.
- `usage`: Object containing token metrics for the cancelled turn:
  - `input_uncached`: Uncached input tokens.
  - `cache_write`: Tokens written to prompt cache.
  - `output`: Output tokens generated.
  - `budgeted`: Budget metric: `(input_tokens - cached_tokens) + output_tokens`. Excludes prompt cache reads.
- `thinking_excerpt`: String containing captured thinking text prior to cancellation, or `null` if none was captured.
- `retry_hint`: Human-readable text explaining how to retry the session.

### Execution & Limitation Details

- **Budget metric & scope**: Token budget applies per invocation as a fresh delta. Each retry starts with a clean budget allowance. Workers evaluate token usage at turn boundaries before calling the model, so at least one model call executes when `token_budget > 0`.
- **Caller-side timeout vs worker-side budget**: `--timeout-secs` is enforced on the caller side. If the CLI process disconnects or terminates unexpectedly, the caller-side timeout timer stops, leaving any detached worker running. In contrast, `--token-budget` is enforced worker-side before model calls, bounding costs even if a worker becomes orphaned.
- **Thinking excerpt limitation**: Non-streaming model calls do not yield partial thinking text during an active request. A mid-call timeout on a non-streaming request produces an empty thinking excerpt ("none captured"). Budget exhaustion triggers at turn boundaries and can include thinking text when streaming is enabled.

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
Web UI:                http://127.0.0.1:8000/
Embeddings API (POST): http://127.0.0.1:8000/v1/embeddings
Rerank API (POST):     http://127.0.0.1:8000/v1/rerank
```

Open the Web UI URL in a browser (requires the web-ui assets — installed by
`cargo xtask install`, or pass `--web-assets <dir>`). The API lines below it are
POST-only endpoints for programmatic use. When binding to a wildcard host (e.g.
`0.0.0.0`), the printed URL uses loopback (`127.0.0.1`) so it's directly clickable.

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
