# Script Tools Guide

Script tools are user-defined, agent-facing tools implemented as in-process
BashKit Bash or Monty Python scripts. They allow you to wrap, filter,
transform, or compose existing native, MCP, and tool-server tools while
leaving the original tools accessible.

Unlike command templates in `harnx-bash-tools` (which run as sandboxed host
subprocesses and cannot invoke other tools), script tools run inside the worker
process and can call allowlisted tools directly. Unlike lifecycle event hooks
and tool middleware (which intercept session events or wrap tool dispatches),
script tools are purpose-built tools exposed directly to the model.

## Configuration

Script tools can be defined in two locations:

1. **Global configuration (`config.yaml`)**: under the top-level `script_tools:`
   key as a list of tool definitions.
2. **Packages (`packages/<pkg>/script_tools/*.yaml`)**: as individual YAML files,
   one definition per file. Only flat `*.yaml` files are loaded; subdirectories
   and other extensions (such as `.yml`) are ignored. The `name:` field declared
   inside the file identifies the tool; the filename stem is not used as a name
   or prefix.

Both definition methods can be used together.

### Definition fields

| Field | Type | Description |
| :--- | :--- | :--- |
| `name` | string | Required. Tool name (1–64 characters from `[A-Za-z0-9_-]`). Must be unique across all sources. |
| `description` | string | Required. Human-readable description exposed in the tool declaration. |
| `parameters` | object | Required. Raw JSON Schema for call arguments. Must declare top-level `type: object`. |
| `tools` | list | Optional list of tool selectors (literal names, globs, or toolsets) the script may call. Defaults to `[]`. |
| `bash` | string | Inline BashKit script source. Mutually exclusive with `python`. |
| `python` | string | Inline Monty Python script source. Mutually exclusive with `bash`. |
| `timeout` | duration | Optional per-run execution timeout (e.g. `300s`, `5m`, `1m 30s`). Defaults to `300s` (5 minutes). |
| `output` | string | Optional output format: `text` (default) or `json`. |

Configuration loading is strict (`deny_unknown_fields`): unknown fields, declaring
both `bash` and `python`, or omitting both will fail configuration loading.
Definitions are read at startup: adding or changing them takes effect after a
config reload or restart.

### Example: global and package definitions

Global definition in `config.yaml`:

```yaml
script_tools:
  - name: fs_preview
    description: Read a file and return its text contents.
    parameters:
      type: object
      properties:
        path:
          type: string
          description: File path
      required: [path]
      additionalProperties: false
    tools: [fs_read]
    bash: |
      path=$(tool_input | jq -r .path)
      fs_read --path "$path"
```

Package definition in `packages/my-pkg/script_tools/fs_digest.yaml`:

```yaml
name: fs_digest
description: Read a file and return a preview of its contents.
parameters:
  type: object
  properties:
    path:
      type: string
      description: Absolute path to the file
  required: [path]
  additionalProperties: false
tools: [fs_read]
output: text
timeout: 60s
python: |
  args = tool_input()
  result = call_tool("fs_read", {"path": args["path"]})
  # The result is the tool's rendered text, not the file verbatim: an empty
  # file reports "empty file", and long reads may carry pagination or
  # truncation notices. Never count it as file lines.
  text = result["content"][0]["text"]
  print(text)
```

## Model exposure and selection

Script tools appear in the tool catalog alongside native and MCP tools.

### Agent selection via `use_tools`

An agent selects script tools using standard `use_tools` selectors in its
Markdown front matter:

- **Literal name**: `use_tools: [gh_issue_brief]`
- **Glob pattern**: `use_tools: ["gh_*", "fs_digest"]`
- **Toolset reference**: if a toolset in `config.yaml` includes the script tool
  name, selecting the toolset exposes the script tool.

When selected, the model sees the script tool's declared name, description,
and parameter schema.

### Script mode via `bash_tools`

When an agent has access to the built-in `bash` tool, script tools can be listed
in `bash_tools`:

```yaml
use_tools: [bash]
bash_tools: [gh_issue_brief]
```

Inside a `bash` script, the script tool is available as a shell command:

```bash
gh_issue_brief --number 2813
```

The execution graph flows `bash -> script tool -> base tools`. Script tools
cannot call `bash` or each other.

Original tools remain callable directly. Adding a wrapper like `gh_issue_brief`
does not replace or disable `bash_gh_issue_view` unless you omit the underlying
tool from `use_tools`.

## Script inputs

Arguments passed by the model are validated against the tool's JSON Schema
before the script runs. Validated arguments are available via host helpers:

- **Bash (`tool_input`)**: Built-in command that prints the validated arguments
  as a JSON string to stdout. It takes no arguments (passing arguments exits 2).
  Parse the JSON with `jq`:
  ```bash
  number=$(tool_input | jq -r .number)
  ```
- **Python (`tool_input()`)**: Host function that returns the validated
  arguments as a Python dictionary. It takes no arguments:
  ```python
  args = tool_input()
  number = args["number"]
  ```

## Calling allowlisted tools

A script tool can only invoke tools matching its `tools:` allowlist.

### Bash bodies

In Bash bodies, each allowlisted tool is available as a shell command with the
same CLI flags, output and exit codes as in the `bash` tool's script mode:

- Parameters map to `--kebab-case` flags (e.g. `--number 2813`). Nullable
  scalars use the same flags, and scalar arrays (including nullable scalar
  arrays) repeat the flag for each value. Passing `null` requires `--json`.
  Objects, nested arrays, and values with multiple non-null types take JSON via
  `--json '<json>'` or `--json -` on stdin.
- A tool that declares a shell form prints like the command it resembles, the
  same as in a `bash` tool script: `fs_read` prints the file's text like `cat`,
  `fs_ls` and `fs_find` print one name or path per line, and `fs_grep` prints
  `path:line:text` rows. Data goes to stdout and notes go to stderr. `-o json`
  prints the tool's fields instead. See
  [Output in `bash` scripts](../crates/harnx-fs-tools/README.md#output-in-bash-scripts)
  for the file tools in detail, and the same section of the
  [bash tools](../crates/harnx-bash-tools/README.md#output-in-bash-scripts) and
  [fetch tools](../crates/harnx-fetch-tools/README.md#output-in-bash-scripts) READMEs
  for `bash_exec` and the fetchers. Bash bodies get these shell forms too.
- A tool without a shell form prints JSON from `structuredContent` when present,
  or text otherwise. Use `-o json` or `-o text` to override. Text output omits
  content blocks not addressed to the assistant.
- Commands exit `0` on success; `1` on a tool error; `2` on a usage or flag
  error, or when a tool reports its own error status; `126` when the call needs
  approval (stderr reads `needs approval: <tool>; call it directly instead of
  from bash`, since an inner call can never prompt), is blocked (`blocked:
  <reason>`) or is rejected (`rejected: <reason>`), such as when the run's call
  budget is exhausted; and `127` if the tool is unknown or not allowlisted. On
  exit `1`, any `partial_result` is rendered to stdout with the same output
  format as a successful result.
- `fs_grep` exits `1` when nothing matches.
- `bash_exec` and plain command templates exit with the command's own status,
  so `1` there is the command's and not a tool error: 128 plus the signal
  number for a signal, `124` for the command's own timeout, `130` when a new
  message stopped it, and `125` for a tool error such as a command that could
  not start. `bash_spawn`, `bash_wait` and `bash_status` report a finished
  command the same way. See
  [Output in `bash` scripts](../crates/harnx-bash-tools/README.md#output-in-bash-scripts).
- A tool that changes files reports the change at the level of its `diff`
  option: `none` takes no snapshot and prints nothing (and leaves nothing to
  roll back to), `summary` prints one line, `files` adds git's `--stat` lines
  for up to 50 files, and `full` prints the diff. A script defaults to
  `summary`, where a direct call defaults to `full`. The report goes to stderr
  after the tool's notes, so it never enters a pipe. A summary line is git's
  shortstat and the command that shows the change, with the first 12 digits of
  the snapshot ids. The first id is the snapshot before the change and the
  second is the snapshot just after it:
  `1 file changed, 1 insertion(+), 1 deletion(-): git -C /work/repo diff eb58d8a2f19a 3fd6fdcc1dcb`.
  To undo that change, pipe the line's command into `git apply -R`:
  `git -C /work/repo diff eb58d8a2f19a 3fd6fdcc1dcb | git -C /work/repo apply -R`.
  That touches only the files the change touched and leaves the branch alone.
  `fs_write`, `fs_edit`, `fs_insert` and `fs_re_replace` print a one-line
  confirmation on stdout. `rollback_file` accepts a snapshot id in full or as a
  prefix of at least 7 hex digits, but it restores the whole repository to that
  snapshot and moves the branch, so it is not the way to undo one change.
  Command templates take no `diff` option and follow the defaults.

Under BashKit's default `set -e`, a non-zero exit from an inner tool will halt
the script unless handled with `set +e` or conditional checks (`if`, `||`). For
`fs_grep` that means using it in `if`, with `||`, or adding `|| true`, since a
search with no match is not a failure of the script.

### Python bodies

In Python bodies, call allowlisted tools with `call_tool(name, args)`:

- `name` is the string name of the target tool.
- `args` is a dictionary matching the tool's input schema.
- Keyword arguments are supported: `call_tool(name="fs_read", args={"path": "file.txt"})`.
- On success, `call_tool` returns the tool's output value (dict, list, or primitive).
- On failure (tool error, middleware denial, interaction required, or budget
  exhaustion), `call_tool` raises a `RuntimeError` naming the tool and describing
  the error.

Scripts can catch `RuntimeError` to handle inner tool failures:

```python
try:
    data = call_tool("probe_run", {"value": "test"})
except RuntimeError as err:
    print("inner call failed: " + str(err))
```

Uncaught exceptions stop the script and produce a tool error result.

## Output modes and error handling

A script's stdout determines its tool result. The `output` field configures how
stdout is parsed:

### `output: text` (default)

- The script's stdout becomes the result's text content block.
- Non-empty stderr with an exit code of `0` is preserved in the `stderr` field
  of the result payload; it does not fail the tool call.
- If stdout exceeds the 1 MiB stream capture cap, the output is marked with
  `stdout_truncated: true` and the truncation marker
  `[stdout truncated at the script output cap; discarded output is unavailable]`
  is appended. The tool call still succeeds.

### `output: json`

- The script's stdout must contain exactly one valid JSON value.
- Empty output, multiple JSON values, non-JSON output, or output truncated by
  the 1 MiB cap cause the tool call to fail with a recoverable error. A
  truncated stream fails even when the retained stdout begins with a complete
  JSON value: the cap cut the stream, so that value is not the output.
- On success, the parsed JSON is returned as structured content.

### Failures and error results

When a script fails (non-zero exit code in Bash, uncaught exception in Python,
or timeout), the engine returns a standard error result (`{"is_error": true, "error": ...}`).

The error payload includes:
- The failure status (`exited with code N`, `timed out after Ns`,
  `stopped unexpectedly`, or `cancelled`).
- Bounded stderr from the script run.
- A `partial_result` object containing `status`, `exit_code`, bounded `stdout`,
  bounded `stderr`, the `stdout_truncated`/`stderr_truncated` flags,
  `limit_hit`, any underlying `error` message, and the run's inner-call
  diagnostics: a `trace` of retained inner calls (one row per call with its
  status, bytes and duration), `trace_dropped` when the trace retention cap
  discarded rows, and `failed_call` naming the first failed inner call.

## Schema validation

Each script tool definition requires a `parameters` JSON Schema:

- **Top-level `type: object`**: The root schema must declare `type: object`. A
  missing or non-object top-level type fails configuration loading.
- **Pre-execution validation**: Arguments are validated using `jsonschema`
  before the script runs. Invalid arguments return a recoverable error result
  naming the violation and instance path without executing the script.
- **No `default` argument injection**: JSON Schema `default` properties are
  preserved in the schema for model guidance, but they are not injected into
  call arguments when omitted by the caller.
- **No remote schema references**: External `$ref` resolution over HTTP or the
  host filesystem is disabled at schema compilation. Local schema definitions
  (e.g. `"$ref": "#/$defs/my_def"`) work normally.
- **Dialect and `format`**: Validation uses the `jsonschema` crate's default
  options. A schema without `$schema` is treated as Draft 2020-12, where
  `format` is annotation-only; `format` is asserted only when the schema
  declares Draft 4, 6, or 7. Declare the dialect explicitly if you rely on
  `format` assertions.
- **Timing and cost**: Argument validation runs before the script starts and
  before the per-run timeout begins counting; it has no separate size or
  depth budget beyond the schema itself.

## Allowlist, exclusions, and collisions

### Excluded tools

Script tools cannot call:
1. `bash`
2. Control tools: `switch_agent`, `ask_user`, and all `*_session_handoff` tools.
3. Other script tools (including recursive calls to themselves).

These restrictions are enforced at two levels:
- **Config load**: A literal selector in `tools:` naming an excluded tool or a
  script tool fails configuration loading.
- **Runtime dispatch**: Wildcards and globs (such as `probe_*` or `*`) silently
  skip excluded tools and script tools.

### Name rules and reserved names

- Tool names must contain 1 to 64 ASCII characters from `[A-Za-z0-9_-]`.
- Reserved names are rejected at config load: `bash`, `switch_agent`,
  `ask_user`, any pattern matching `*_session_handoff`, and built-in session/meta
  tools (`session_history`, etc.).
- Names must be unique across global configuration and all package definitions.
  A duplicate name fails configuration loading and reports both source locations.

### Base-tool name collisions

If a script tool shares a name with an existing base tool (a built-in tool,
native NATS toolset tool, or discovered MCP tool):
- The original base tool takes precedence and remains callable.
- The script tool is disabled (it is not advertised in tool declarations and
  cannot be dispatched).
- A warning is logged once naming the colliding script tool and its location.
- Collision checks re-evaluate whenever tool declarations refresh (such as
  after NATS tool server registration).

## Human approval and inner calls

Script tools interact with tool confirmation and middleware as follows:

- **Top-level approval**: A script tool can be confirmed or intercepted by
  middleware using its own declared name (e.g. `match: '^gh_issue_brief$'`).
- **Inner-call approval**: Inner calls made from inside a script cannot prompt
  for interactive human-in-the-loop (HITL) approval. If an allowlisted tool or
  its middleware requires confirmation, the inner call is denied immediately:
  - In Bash, a tool requiring approval exits `3` with stderr
    `needs approval: <tool>; call it directly instead of from bash`. A
    middleware interaction that cannot be answered inside a script exits `4`
    with stderr `interaction required, can't be answered inside a script`.
  - In Python, `call_tool` raises a `RuntimeError` containing
    `interaction required for '<tool>': ...`.
- **Agent middleware**: Configured tool middleware applies to inner calls. A
  middleware denial causes the inner command to exit non-zero (Bash) or raise
  `RuntimeError` (Python).

## Resource bounds and limits

Script tools execute within the in-process BashKit / Monty virtual environment
under the following limits:

- **Timeout**: Bounded by `timeout` (default `300s`). Timeouts are whole
  seconds: sub-second values are truncated, and anything below one second
  runs with a 1-second limit. When the timeout expires, the script
  terminates, pending inner calls are cancelled, and the tool returns an
  error.
- **Cancellation**: If the session or turn is cancelled (e.g. via abort signal
  or Ctrl+C), the script terminates and pending inner calls are cancelled.
  Native interpreter operations observe cancellation at their next boundary,
  not instantly; the BashKit parser, memory, job, and VFS limits retain their
  defaults (see `crates/harnx-script/README.md`).
- **Output caps**: Captured stdout and stderr are capped at 1 MiB each.
- **Command limit**: Bash scripts can execute at most 10,000 commands per run.
- **Concurrency cap**: At most 8 inner tool calls can execute in parallel.
- **Call budget**: A script can execute at most 1,000 inner tool calls per run.
  Calls beyond this budget are rejected.
- **Nesting multiplies budgets**: The budget, concurrency cap, and timeout
  are per script-tool run, and `bash` calling script tools composes them. One
  `bash` call is itself limited to 1,000 inner calls, but each script tool it
  invokes opens a fresh run: at most 8 of those script tools run concurrently
  (the `bash` run's own cap), each with 8 inner-call slots and a 1,000-call
  budget of its own. In the worst case a single `bash` call can therefore
  drive up to 1,000,000 leaf tool calls with up to 64 in flight at once
  (8 script tools × 8 inner calls), each bounded by its own `timeout`.
- **Virtual filesystem**: Scripts operate in an ephemeral in-process virtual
  filesystem with no direct host filesystem, subprocess, or network access.

## Where script tools run

Script tool bodies need an evaluation context. Lifecycle event hooks, operator
`.call` commands, non-Bash tool-reservation calls, and orphan repair have no
script evaluation slot, so they neither advertise nor dispatch script tools.
Operator and reserved `bash` commands install a slot and can use selected script
tools. In contexts without a slot, selectors that would match a script tool
resolve to nothing instead of failing at call time. This does not restrict what
a *running* script tool may call; it only limits where script tools themselves
can be invoked from.

## Managed tool server startup

When an agent selects a script tool (via `use_tools` or `bash_tools`), Harnx
inspects the script tool's `tools:` selectors and automatically starts any
locally managed tool servers required by those inner tools on session
activation.

You do not need to list the wrapped inner tools in the agent's `use_tools`
to trigger server startup. If a script tool is not selected by the active agent,
its inner tool servers are not started.

The startup closure covers only the agent's `use_tools` and `bash_tools`
selections. Script tools reached through some other path — middleware `tools:`
selectors, for example — do not trigger managed startup; add such tools to
`use_tools` or `bash_tools` as well if their inner servers need starting.
