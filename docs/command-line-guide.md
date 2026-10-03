# Command Line Guide

## Usage

```
Usage: harnx [OPTIONS] [COMMAND]

Commands:
  prompt   Run a non-interactive prompt
  info     Inspect harnx state
  dump     Dump resources
  open     Open resources in the system application
  delete   Delete resources
  list     List resources
  call     Call tools directly
  compact  Compact session logs to reduce history size
  help     Print this message or the help of the given subcommand(s)

Options:
      --timeout-secs <SECONDS>  Maximum one-shot invocation duration in seconds (0 or unset means no limit)
      --token-budget <TOKENS>   Maximum budgeted tokens for one-shot invocation (0 or unset means unlimited)
  -h, --help                    Print help
  -V, --version                 Print version
```

## Examples

```
harnx                                          # Enter the interactive TUI
harnx prompt Tell a joke                       # Generate response
harnx -- Tell a joke                           # Same, using an explicit separator

harnx-serve                                    # Run server (standalone binary)
harnx-serve --addr 0.0.0.0:8080                # Run server with addr

harnx-mcp-bridge --list-tools -- my-server      # Inspect external MCP tools (standalone binary)
harnx-mcp-bridge --call-tool query --tool-args '{"q": "test"}' -- my-server

harnx -m openai:gpt-4o                         # Select LLM

harnx -s                                       # Begin a temp session
harnx -s session1                              # Use session 'session1'
harnx -a agent1                                # Use agent 'agent1'
harnx --rag rag1                               # Use RAG 'rag1'

harnx info agent agent1                        # View agent info
harnx info session agent1 session1             # View session metadata
harnx dump session agent1 session1             # Dump session transcript
harnx dump session agent1 session1 --follow    # Follow transcript live
harnx dump session agent1 session1 --check-loop-detection  # Show where loop protection would step in
harnx dump attachment cid:media:...            # Dump attachment text to stdout
harnx dump attachment cid:media:... --output file.bin  # Dump attachment to file
harnx open attachment cid:media:...            # Open attachment in default viewer
harnx list sessions                            # List sessions
harnx compact session agent1 session1          # Compact session log
harnx delete session session1 --agent myagent --cluster local  # Delete session
harnx list tools                               # List all available tools
harnx list tools "fs_*"                        # List tools matching selector pattern
harnx list tools --json                        # List all tools as JSON array
harnx info tool fs_read                        # View tool metadata and input schema
harnx info tool fs_read --json                 # View tool declaration as JSON
harnx call tool fs_read '{"path": "file.txt"}' # Call tool directly with JSON arguments
harnx call tool fs_read '{"path": "file.txt"}' --json  # Return full tool result as JSON
harnx -a agent1 call tool fs_read '{"path": "file.txt"}' # Call tool using agent allowlist
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

Ctrl+C during a one-shot prompt appends an interrupt to the session log and
exits as soon as the broker acknowledges that append. The CLI does not wait for
the worker to stop its tools or wind the turn up; the worker does that on its
own, and tying the exit to it would make a dead worker hang the terminal. A
one-line summary on stderr says whether there was a turn to stop, and an append
that fails is reported as an error rather than a silent exit. A `--timeout-secs`
expiry interrupts the same way but keeps its own exit contract below.

## Bounding a One-Shot Run

Non-interactive prompts (`harnx prompt` or `harnx -- <text>`) can set per-invocation controls using `--timeout-secs` and `--token-budget`. Worker-owned run deadlines also apply to TUI and Web UI admissions, with a finite 24-hour fallback when no positive policy is configured.

- `--timeout-secs <SECONDS>`: A positive value sets the admission allowance and a caller-side observer timer. Passing `0` or omitting the option leaves that observer timer off; the worker still uses target/global policy and the finite 24-hour fallback.
- `--token-budget <TOKENS>`: Maximum budgeted tokens for the invocation. Passing `0` or omitting the option means unlimited tokens.

### Limit Exhaustion Behavior

The two limits end the turn differently. A `--timeout-secs` expiry is a
caller-side interrupt: the CLI appends a durable `Cancel` to the session log,
which terminates the turn wherever the worker is. `--token-budget` is enforced
worker-side at a round boundary, before a model call, so it ends the turn with
an `Error` entry instead — nothing is cut off mid-flight, and there is no
interrupt to deliver.

Either way, the caller-facing behaviour is the same:
1. Harnx writes a synthesized human-readable explanation to `stdout`.
2. Harnx writes a single compact JSON line to `stderr`.
3. The process exits with code **2** (distinct from generic error exit code 1).

If `--final-only` is active, normal startup headers and progress lines are suppressed, but on limit exhaustion Harnx still prints the synthesized text to `stdout` and the JSON line to `stderr`.

A turn can also be stopped without either flag. When loop protection (see
[Loop Detection](configuration-guide.md#loop-detection)) ends the turn because
the model kept making the same tool call with the same result or kept
repeating the same text, the one-shot run reports it like a budget stop: the
synthesized explanation on `stdout`, one JSON line on `stderr` with `kind` set
to `"repetition"`, and exit code **2**.

### Stderr JSON Interface

The single stderr JSON line provides a stable, machine-readable contract for downstream scripts and tooling:

```json
{"kind":"timeout","session_id":"01948a3f-7b1c-7123-8901-abcdef123456","usage":{"input_uncached":120,"cache_write":0,"output":45,"budgeted":165},"thinking_excerpt":null,"retry_hint":"Inspect saved public results before sending revised or narrower instructions to the same session id `01948a3f-7b1c-7123-8901-abcdef123456` within a live run. Do not retry unchanged."}
```

A repetition stop adds `source` after `retry_hint`, and for repeated tool calls `tool` and `count` as well:

```json
{"kind":"repetition","session_id":"01948a3f-7b1c-7123-8901-abcdef123456","usage":{"input_uncached":120,"cache_write":0,"output":45,"budgeted":165},"thinking_excerpt":null,"retry_hint":"Inspect saved public results and change the repeated approach before continuing the same session id `01948a3f-7b1c-7123-8901-abcdef123456` within a live run. Do not retry unchanged.","source":"tool_calls","tool":"fs_read","count":4}
```

Field reference:
- `kind`: `"timeout"`, `"budget_exceeded"` or `"repetition"`.
- `session_id`: Session ID of the cancelled turn. Pass this ID together with the same explicit `--agent` on a subsequent prompt command to retry in the same session.
- `usage`: Object containing token metrics for the cancelled turn:
  - `input_uncached`: Uncached input tokens.
  - `cache_write`: Tokens written to prompt cache.
  - `output`: Output tokens generated.
  - `budgeted`: Budget metric: `(input_tokens - cached_tokens) + output_tokens`. Excludes prompt cache reads.
- `thinking_excerpt`: String containing captured thinking text prior to cancellation, or `null` if none was captured.
- `retry_hint`: Human-readable text explaining how to retry the session.
- `source`, `tool`, `count`: `source` is present only when `kind` is `"repetition"` and says what kept repeating: `"tool_calls"`, `"answer"` or `"thinking"`. `tool` and `count` appear only with `"tool_calls"`: `tool` is the name of the repeated tool, and `count` is how many identical calls with identical results had run within the 10-minute window when the turn was stopped.

### Execution & Limitation Details

- **Budget metric & scope**: Token budget applies per invocation as a fresh delta. Each retry starts with a clean budget allowance. Workers evaluate token usage at turn boundaries before calling the model, so at least one model call executes when `--token-budget` is positive.
- **Worker-owned deadlines & CLI budgets**: `--timeout-secs` sets an admission deadline on the worker session while also maintaining a caller-side observer timer. Detached workers enforce this deadline independently and terminate even if the CLI process disconnects. In contrast, `--token-budget` is an independent token accounting limit evaluated worker-side before model calls for non-interactive CLI prompts.
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
`cargo xtask install`, or pass `--web-assets <dir>`). The flag also reads
`HARNX_WEB_ASSETS` if unset; the harnx container image sets this env var
automatically. See [Deploy Harnx on Kubernetes](kubernetes-deployment.md)
for containerized deployments. The API lines below it are
POST-only endpoints for programmatic use. When binding to a wildcard host (e.g.
`0.0.0.0`), the printed URL uses loopback (`127.0.0.1`) so it's directly clickable.

Common flags:

```sh
harnx-serve --addr 0.0.0.0:8000
harnx-serve --model claude:claude-3-5-sonnet-20240620
harnx-serve --dry-run
harnx-serve --agent-variable env production --agent-variable debug true
```

## Inspect Agents

Use `harnx info agent` to inspect rendered agent configuration.

Session IDs are local to an agent and case-sensitive. Different agents can each
use `review-12345`. Always supply the agent when addressing a session, including
`harnx --agent alpha --session review-12345 prompt "Continue the review"`,
`harnx delete session review-12345 --agent alpha --cluster local`, and the TUI
command `.info session alpha review-12345`. No agent is inferred for these commands.
For remote session inspection, use an explicit selector such as
`harnx info session alpha@prod review-12345`.

### `harnx info agent <name>`

Prints the fully-rendered agent configuration to stdout. This includes:
- YAML front-matter with package patches applied and `use_tools` wildcards expanded to concrete tool names via live MCP servers.
- The system prompt with all variables and templates (MiniJinja) interpolated.

If an MCP server fails during tool expansion, a warning is logged to stderr, and the command continues with the remaining tools.

## Inspect and Dump Sessions

### `harnx info session <agent-name> <session-id> [--format text|yaml|json]`

Prints saved session metadata only to stdout.

> **Behavior Change:** `harnx info session` no longer prints the transcript. It outputs session metadata only. To dump the transcript like earlier versions did, use `harnx dump session <agent-name> <session-id> --format yaml`.

Options:
- `--format text` (default): Prints human-readable session summary metadata (model, title, token usage, turns, settings) matching TUI `.info session`.
- `--format yaml`: Prints the full `SessionMetadata` record as a single YAML document (includes `variables` and config overrides).
- `--format json`: Prints the full `SessionMetadata` record as a single JSON object.

This command does not output transcript entries, does not include the system prompt, and does not launch MCP servers.

### `harnx dump session <agent-name> <session-id> [--format text|yaml|json] [--follow | --check-loop-detection]`

Dumps the session transcript (history).

Formats:
- `--format text` (default): Syntax-highlighted, human-readable output matching one-shot `harnx prompt` rendering. Reconstructs messages and tool calls/results into clean terminal output.
- `--format yaml`: Prints all durable transcript entries as `---`-separated `SessionLogEntry` YAML documents (includes messages, tool calls, tool results, and control entries).
- `--format json`: Prints transcript entries as **JSONL** (JSON Lines) with exactly one `SessionLogEntry` JSON object per line. This is NOT a single JSON array `[...]` — each line is an independent JSON object, making it streamable and directly parseable with `jq -c` or line-by-line scripts without loading the entire transcript into memory at once.

Live tail with `--follow`:
- `--follow`: Runs a `tail -f`-style live follow mode. Replays existing history to stdout, then listens for newly committed durable entries and streams them until interrupted with `Ctrl-C`.
- Read-only observation: `--follow` observes the session stream and does not interrupt, cancel, or modify the running session or agent.

Loop check with `--check-loop-detection`:
- `--check-loop-detection`: Prints no transcript. Replays the session's tool calls through harnx's loop protection (see [Loop Detection](configuration-guide.md#loop-detection)) and reports each call that would have got a note, been refused, or ended the turn. Before comparing results, the replay removes the notes harnx added to them and ignores the results of calls harnx refused or stopped, so it works on a session recorded with loop protection on as well as one recorded without it. The replay starts counting again at each user message, compaction and turn end, the points where the live guard starts over. It also checks each saved reply's answer and reasoning against the [repeated-output rule](configuration-guide.md#repeated-output) and reports each reply the rule would have stopped.
- `--format text` (default): One line per event: the log sequence number of the entry that requested the call, the time in UTC (`-` when the log has none), what happened (`note`, `refused` or `turn stopped`), the tool, its arguments cut to 120 characters, and how many identical calls the guard counted. Each reply the repeated-output rule would have stopped gets a line too, with the sequence number and time of the entry that holds the reply, then the channel, the repeated text cut to 120 characters, its length, and the character at which the repeat was found. Two summary lines follow, then the first three limits below.

  ```
  ...
  seq 158 2026-09-30 04:38:31 note: fs_read {"offset":70,"limit":70,"path":"crates/harnx-tui/src/subagent_sessions.rs"} (4 identical)
  seq 162 2026-09-30 04:38:36 refused: fs_read {"limit":70,"offset":70,"path":"crates/harnx-tui/src/subagent_sessions.rs"} (4 identical)
  seq 164 2026-09-30 04:38:38 note: fs_read {"offset":140,"limit":30,"path":"crates/harnx-tui/src/subagent_sessions.rs"} (3 identical)
  seq 166 2026-09-30 04:38:40 refused: fs_read {"offset":70,"path":"crates/harnx-tui/src/subagent_sessions.rs","limit":70} (4 identical)
  seq 168 2026-09-30 04:38:42 turn stopped: fs_read {"path":"crates/harnx-tui/src/subagent_sessions.rs","offset":70,"limit":70} (4 identical)
  seq 171 2026-09-30 04:39:02 repeated answer: "the " (unit of 4 chars, caught at char 2048)
  683 tool calls: 14 notes, 2 refusals, 1 stops.
  1 repeated outputs (1 answer, 0 thinking).
  Replay limits: after a refusal the real session may have run the call and gone on, so later events may differ from a live run; after a stop the replay skips to the next turn; a response stopped for repeating itself was never saved, so only saved replies are checked for repeated output.
  ```

- `--format json`: Prints one JSON object, not JSONL: `{"tool_calls": <n>, "events": [...], "output_events": [...]}`. Each event has `seq`, `timestamp` (`null` when the log has no time for the call), `kind` (`"note"`, `"refusal"` or `"stop"`), `tool`, `arguments` and `count`. Each output event has `seq`, `timestamp` (`null` when the log has no time for the reply), `channel` (`"answer"` or `"thinking"`), `unit` (the repeated text, cut to 120 characters), `unit_len` and `at_char`. `tool_calls` counts every call in the log, including the calls after a stop that the replay did not decide.
- `--format yaml` and `--follow` are rejected with this flag.

Limits of the replay, in both formats (the text output prints the first three; the JSON output states none of them):
- In a session recorded without loop protection, a refusal in the replay did not happen in the real session. The call ran, the model saw its real result and carried on, so events after a refusal can differ from what a live run would have produced.
- After a stop, the replay does not decide the rest of that turn's calls. It picks up again at the next turn.
- A response the output guard stopped was never saved, so the output check sees only saved replies. It finds repeated replies in sessions recorded before the output guard existed or with it turned off.
- With clients that stream reasoning inline in `<think>` tags, such as `openai-compatible` and `llama-server`, the live guard sees a reasoning loop as a repeating answer and reports it as `answer`. The replay splits the saved `<think>` block out of the reply, so it reports the same loop as `thinking`.
- The rules apply whatever `loop_detection` was set to when the session ran.

### `harnx dump attachment <url> [--output <path>]`

Dumps an attachment or plan document to stdout or a file. Supports canonical `cid:media:` (attachments) and `cid:plan:` (rendered plan markdown) URLs.

- **Text MIME types**: Outputs directly to stdout. This applies to `text/*`, `application/json`, `application/xml`, `application/yaml`, `application/toml`, `application/sql`, `application/graphql`, `application/javascript`, and rendered plan markdown.
- **Binary MIME types without `--output`**: Fails with an error message directing you to use `harnx open attachment <url>` or supply `--output <path>`.
- **`--output <path>`**: Writes output bytes directly to the given destination path. This flag uses the long form only; `-f` is reserved for prompt input files.

### `harnx open attachment <url>`

Opens an attachment or plan document in the system default application. Supports `cid:media:` and `cid:plan:` URLs.

- Persists the attachment payload to a uniquely-named temporary file (`harnx-attachment-*.ext`) with a file extension derived from its MIME type.
- Filters unsafe executable extensions (`.bat`, `.cmd`, `.com`, `.exe`, `.vbs`, `.vbe`, `.js`, `.jse`, `.wsf`, `.wsh`, `.scr`, `.ps1`, `.sh`, `.bash`), defaulting them to `.bin`.
- Launches the system default opener in a detached background process so the command returns immediately.

### `harnx list sessions`

Lists available sessions as tab-separated agent names and session IDs, one session per line. `<inline>` identifies sessions without a named agent. For local agents, lists sessions in the local NATS store. When targeting a remote agent via `--agent <remote-agent>`, lists sessions in that remote cluster. (Replaces the deprecated `--list-sessions` flag.)

### `harnx delete session <session-id> --agent <agent> --cluster <cluster>`

Deletes the specified agent’s NATS session from the specified cluster. If the agent selector includes `@cluster`, it must match `--cluster`. (Replaces the old `harnx session delete` command.)

### `harnx compact session <agent-name> <session-id> [--timeout <seconds>]`

Submits a manual compaction request for a session and blocks until it completes, streaming progress advisories.

If compaction is already in progress, the command attaches to the existing operation and waits for its outcome.

Options:
- `--timeout <seconds>`: Maximum seconds to wait for compaction completion (default: `60`, pass `0` to wait indefinitely).

Exit status:
- Exits **0** on successful compaction.
- Exits **0** when there is nothing to compact (the session does not have enough uncompacted messages or tokens to warrant summarization).
- Exits **nonzero** if compaction fails or the timeout expires.

### `harnx list tools [<pattern>] [--json]`

Lists available tools, optionally filtered by a selector pattern.

- **`<pattern>`**: Optional tool or toolset selector (for example, `fs_*`, `packaged_read`, or `*`). If omitted, lists all available tools.
- **`--json`**: Outputs a JSON array of complete tool declarations (`ToolDeclaration` objects including `name`, `description`, `parameters`, `mcp_tool_name`, `mcp_server_name`, `call_template`, `result_template`, `idempotent_hint`, `read_only_hint`, and `kind`).
- **Default output**: Human-readable format listing each tool with its name, description, runtime metadata, and formatted JSON input schema. If no tools match, prints `No tools found.`.
- **Targeting an agent**: Accepts `-a <agent>` / `--agent <agent>` (including `<agent>@<cluster>`) to list tools available under that agent's configuration and allowlist.

### `harnx info tool <name> [--json]`

Inspects the metadata and parameter schema for a specific tool.

- **`<name>`**: Exact name of the tool to inspect.
- **`--json`**: Outputs the complete tool declaration as JSON.
- **Default output**: Human-readable display showing the tool name, description, runtime metadata, and formatted JSON input schema.
- **Targeting an agent**: Accepts `-a <agent>` / `--agent <agent>` to inspect tools available in that agent's context.
- **Exit status**: Exits **0** if the tool is found, or **1** if the tool is not found or not available in the selected context.

### `harnx call tool <name> <args-json> [--json]`

Executes a tool directly without running an LLM model turn or creating an inference turn.

- **`<name>`**: Exact tool name to execute.
- **`<args-json>`**: Tool arguments as a JSON object, passed as a single quoted shell argument (for example, `'{"path": "README.md"}'`).
- **`--json`**: Outputs the complete tool result payload as JSON. Preserves all result structures, including text blocks, images, binary attachments, structured content (`structuredContent`), error flags (`isError`), partial indicators (`partial`), and extension metadata (`_meta`).
- **Default output**: For plain text string results, prints the text content directly; for structured or complex results, prints pretty-printed JSON.

#### Flag Placement and Agent Selection

- **`--json` placement**: `--json` is a subcommand-level option for `info tool`, `list tools`, and `call tool`. Place it after the command arguments:
  ```sh
  harnx info tool fs_read --json
  harnx list tools "fs_*" --json
  harnx call tool fs_read '{"path": "file.txt"}' --json
  ```
- **`--agent` / `-a` placement**: Can be placed before the subcommand or after it:
  ```sh
  harnx -a coder info tool fs_read
  harnx info tool fs_read --agent coder
  harnx call tool fs_read '{"path": "file.txt"}' -a coder@cluster
  ```
- **With `--agent`**: Resolves tools using the named agent's configured toolsets, packages, allowlist, and NATS cluster. Tool execution runs on the agent's worker and appends subagent progress to an empty caller transcript, without running a root model turn.
- **Without `--agent` (no-agent mode)**:
  - Direct tool inspection and invocation do not default to an arbitrary agent.
  - Instead, the CLI opens a virtual-session tool reservation (`ToolReservationHandle`) scoped strictly to the requested tool or pattern.
  - The CLI runs a strict discovery scan with a private catalog, confirms scope admission, and watches for cluster changes.
  - When the command finishes, the CLI explicitly closes the reservation before terminating the local worker. If connection to the broker is lost, release is best-effort and the reservation is reclaimed by its server-side TTL.

#### Argument Validation and Shell Quoting

The `<args-json>` argument must parse to a JSON object (`{...}`). Validation occurs immediately during CLI argument processing, before loading configuration, launching background workers, or reserving tools. Passing invalid JSON or a non-object JSON value (such as arrays, numbers, strings, or booleans) fails immediately with exit status 1.

Always quote the argument so your shell does not split on spaces or interpret quotes:
```sh
# Correct: quoted JSON object
harnx call tool bash_exec '{"command": "echo \"two words\""}'

# Fails validation before worker startup (not an object):
harnx call tool bash_exec '["echo", "two words"]'
```

#### Timeouts and Cancellation

- **`--timeout-secs <SECONDS>`**: Sets a maximum execution duration for the command.
- **Default timeout**: Unlike `harnx prompt` (where 0 or omitting the flag means unlimited execution), tool commands use a bounded default timeout of **600 seconds** (10 minutes) when `--timeout-secs` is omitted or passed as `0`.
- **Cancellation (`Ctrl+C`) and timeout expiration**:
  - Sets an internal abort signal and allows a 10-second drain window for in-flight calls to settle.
  - If output was produced during the drain, it is emitted to stdout.
  - The command terminates with exit code **1** and prints an advisory to stderr:
    - `error: Operator tool command cancelled; outcome may be unknown`
    - `error: Operator tool command timed out; outcome may be unknown`
  - Tool execution is never automatically replayed. Because the external tool may have partially executed or completed before cancellation settled, the command reports an unknown outcome rather than claiming success.

#### Error Handling and Output Contracts

- **Exit codes**: Returns **0** on successful execution; returns **1** on any failure (argument validation error, missing tool, tool execution error, hook rejection, timeout, or reservation failure).
- **`--json` parseability**: In `--json` mode, stdout remains valid, parseable JSON on all exit paths:
  - If the tool provider returned an error or partial result, that exact JSON payload is written to stdout.
  - If failure occurred before a tool result was produced (for example, bad arguments, invalid configuration, or discovery errors), stdout contains `{"isError": true, "error": "<message>"}`.
  - In all failure cases, a human-readable diagnostic is printed to stderr (`error: <message>`).
- **Partial results**: If a tool returns a payload flagged with `"partial": true` or `"resultType": "partial"`, Harnx emits the payload but treats the call as a failure (exit code 1). Partial execution is never reported as success.

#### Scoped Consent and Approval Isolation

Tool execution through `harnx call tool` runs with scoped operator consent:

- **Root operator invocation auto-approval**: When an operator runs `harnx call tool`, any `PreToolUse` hook that requests approval (`"permissionDecision": "ask"`) is automatically approved for that specific tool call ID. Because the human operator directly initiated the call, no second interactive approval prompt is presented.
- **Hook `deny` enforced**: A `PreToolUse` hook that returns `"deny"` (or exits with code 2) is strictly enforced. The tool is blocked immediately, returning a blocked tool result (`"blocked_by_hook": true`) and exiting with code 1.
- **Hook mutations and schema validation**: Any argument mutations returned by hooks (`mutatedToolInput`) are applied before execution, and tool input schemas are validated normally.
- **Nested approval isolation**: Root consent applies only to the root tool call ID. If the invoked tool spawns nested sub-agents or secondary tool calls, those nested calls do not inherit root approval. In the non-interactive CLI environment, nested approval requests safely defer or decline rather than executing unconfirmed.

## Standalone MCP Bridge Inspection and Invocation (`harnx-mcp-bridge`)

`harnx-mcp-bridge` wraps external stdio-based Model Context Protocol (MCP) servers and exposes them over NATS. For diagnostics and scripting, the bridge also supports standalone tool listing and direct tool invocation without requiring NATS.

### Syntax

All bridge options must precede the `--` separator. The command and arguments that launch the child MCP server follow `--`:

```sh
# List tools advertised by an external MCP server
harnx-mcp-bridge --list-tools [--enable-tool <glob>] [--name <server-name>] -- <command> [args...]

# Invoke a tool directly
harnx-mcp-bridge --call-tool <tool-name> [--tool-args <json-object>] [--enable-tool <glob>] [--name <server-name>] -- <command> [args...]
```

### Options

- **`--list-tools`**: Starts the wrapped server, performs the initial MCP handshake, prints the advertised tools (including descriptions and hints), and exits. Incompatible with `--call-tool`.
- **`--call-tool <name>`**: Starts the wrapped server, invokes the named tool with arguments, prints the full JSON result to stdout, and exits. Incompatible with `--list-tools`.
- **`--tool-args <json-object>`**: Arguments for the tool call as a JSON object (for example, `'{"timezone": "UTC"}'`). Defaults to `{}` if omitted. Requires `--call-tool`.
- **`--enable-tool <glob>`**: Repeatable filter pattern. When specified, only matching tools can be registered, listed, or called. If `--call-tool` targets an excluded tool, execution is rejected before invocation.
- **`--name <server-name>`**: Server name. Required when serving over NATS; optional for standalone inspection and invocation (defaults to `mcp-diagnostic`).

### Argument Validation and Quoting

The `--tool-args` value must parse to a JSON object (`{...}`). Validation occurs before spawning the child process:
- Passing invalid JSON or non-object JSON values (arrays, strings, numbers, booleans, or null) fails immediately with exit status 1 and prints an error to stderr.
- Quoting the JSON argument prevents shell word-splitting.

```sh
# Correct: quoted JSON object
harnx-mcp-bridge --call-tool search --tool-args '{"query": "rust"}' -- npx -y @modelcontextprotocol/server-everything

# Rejected before spawn (array instead of object):
harnx-mcp-bridge --call-tool search --tool-args '["rust"]' -- npx -y @modelcontextprotocol/server-everything
```

### Output and Error Contracts

- **Successful execution**: Prints the complete MCP `CallToolResult` JSON object to stdout and exits with status **0**.
- **Tool-reported failure (`isError: true`)**: Prints the complete `CallToolResult` JSON object to stdout, prints `error: tool '<name>' reported isError: true` to stderr, and exits with status **1**. Complete response payloads (text, images, embedded resources, structured content, and metadata) are preserved.
- **Non-complete results**: If a tool returns a payload containing a `resultType` other than `"complete"` (such as `"partial"`), the command exits with status **1**.
- **Protocol, transport, and discovery errors**: If the child fails to start, the handshake fails, the tool is not found, or the transport disconnects, the error is written to stderr and the command exits with status **1**. No fabricated tool result is emitted to stdout.

### Standalone Mode and Environment Isolation

- **No NATS connection**: Standalone `--list-tools` and `--call-tool` invocations do not connect to NATS or publish registrations.
- **Environment isolation**: Before spawning the child process, `harnx-mcp-bridge` strips `HARNX_SERVER_SCOPE`, `HARNX_NATS_URL`, and `HARNX_NATS_TOKEN` from the child environment. If the wrapped child command itself requires NATS (such as a native Harnx tool server running in stdio mode), pass those variables explicitly to the child command (for example, `-- env HARNX_NATS_URL=nats://127.0.0.1:4222 harnx-time-tools --mcp-stdio`).
- **Session-identity limitations**: Tools that require caller session identity (such as attachment creation tools) fail clearly with an error when run through the stdio bridge, because stdio bridge invocations have no session identity context.
- **Process cleanup**: The bridge manages the child process with `ChildProcessManager`. The child is explicitly stopped and reaped on all exit paths (success, tool error, protocol failure, or startup error), with `kill_on_drop` fallback to prevent orphaned processes.
