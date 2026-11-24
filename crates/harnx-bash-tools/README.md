# harnx-bash-tools

`harnx-bash-tools` runs shell commands through a filesystem sandbox. Native toolset mode is the default; `--mcp-stdio` keeps MCP stdio compatibility.

Filesystem access is deny-all unless explicit allow paths or batches are enabled. Write and execute grants also grant read access. `$HOME` and its ancestors are never writable or executable through allow inputs.

## Installation

```sh
cargo install --path crates/harnx-bash-tools
```

## Filesystem allow options

| Option | Description |
| :--- | :--- |
| `--allow-read <PATH>` | Grant read access. Repeatable. |
| `--allow-write <PATH>` | Grant read and write access. Repeatable. |
| `--allow-exec <PATH>` | Grant read and execute access. Repeatable. |
| `--allow-rwx <PATH>` | Grant read, write, and execute access. Repeatable. |
| `--allow-common-default` | Grant common operating-system paths and temporary directories. |
| `--allow-dev-tools` | Grant supported development toolchains and caches. |
| `--allow-repo-work` | Grant detected project paths and session working directory. |
| `--allow-all` | Request full filesystem access, subject to `$HOME` ancestor guard. |

Other options include `--tool <PATH>` (load one YAML command template; repeatable), `--tools-dir <PATH>` (load every `.yaml` command template in a directory; repeatable), `--no-sandbox`, `--sandbox-run <PATH>`, `--env`/`-e`, `--name <NAME>`, `--mcp-stdio`, and `--help`/`-h`. `--name` overrides the registered toolset name.

For Streamable HTTP MCP mode, pass `--mcp-http`. `--host` defaults to `0.0.0.0`; `--port` defaults to `3002` and serves MCP at `/mcp`.

## Environment variables

Path-list variables use platform path-list syntax:

- `HARNX_TOOLS_ALLOW_READ`
- `HARNX_TOOLS_ALLOW_WRITE`
- `HARNX_TOOLS_ALLOW_EXEC`
- `HARNX_TOOLS_ALLOW_RWX`

Batch toggles accept `1`, `true`, `yes`, or `on`:

- `HARNX_TOOLS_ALLOW_COMMON_DEFAULT`
- `HARNX_TOOLS_ALLOW_DEV_TOOLS`
- `HARNX_TOOLS_ALLOW_REPO_WORK`
- `HARNX_TOOLS_ALLOW_ALL`

`HARNX_BASH_ENV_PASSTHROUGH` remains a comma-separated list of extra child environment variable names.
`HARNX_PACKAGE_DIR` sets the package directory for auto-discovering templates under `$HARNX_PACKAGE_DIR/bash_tools/`.

## Command deadlines

Foreground `exec` calls and command templates run for up to 86,400 seconds (24 hours) by
default, which is also the maximum. Each call accepts `timeout_secs` from 1 to 86,400 to choose
a shorter deadline; `0` is rejected. Cancelling the tool call still terminates the
foreground command and its process group.

`timeout_secs` is reserved execution metadata for command templates. It is injected into every
template's input schema automatically alongside output controls (`head_lines`, `tail_lines`,
`max_output_bytes`) and must not be declared under the YAML `parameters` map. See
[Reserved template parameters](#reserved-template-parameters) for details.

## Background process retention

Finished `spawn` processes and their cached diffs remain available for up to 30 minutes
from exit. At most 128 finished processes are retained; older exits are dropped first.
Cleanup runs on tool calls, process registration, and process lookup, so an idle server
cleans up on its next call. Running processes are never evicted, and calls already
waiting on a process still return its full result after eviction.

The most recent 1,024 evicted execution IDs report that the process finished and is no
longer tracked. Older evicted IDs return the same error as unknown IDs. Eviction does
not delete log files; `read_exec_log` can still read them.

## Output in `bash` scripts

Inside a `bash` tool script, `bash_exec` and the command templates behave like `bash -c`. The command's stdout goes to the script's stdout, its stderr to the script's stderr, and the script sees the command's exit status. A direct call is unchanged: it returns the model-facing report.

- A signal that ends the command gives 128 plus the signal number. The command's own `timeout_secs` gives 124, and it is not a tool error in a script, so output produced before the deadline still reaches a pipe. A new message that stops the command gives 130. Any error from `bash_exec`, a template, `bash_spawn`, `bash_wait` or `bash_status` gives 125 with the reason on stderr: a command that could not start (an empty command, an unusable `working_dir`, a rejected `env` key), an unknown execution id, or a bridged result over the bridge's limit. `bash_read_exec_log` errors exit 1.
- Scripts run with `set -e`, so a failing command ends the script unless it is used in `if`, followed by `||`, or followed by `|| true`: `bash_exec --command 'grep -q x f' || echo missing`.
- A stream longer than `max_output_bytes` (default 131072) keeps whole lines from its start. A note on stderr, `bash_exec: stdout cut at N of M bytes; full log: bash_read_exec_log --execution-id ID --stream stdout --offset K`, says so, names the log to read and the line to continue from; `--offset` is left off when a single over-long first line was cut mid-line, and from a wait or status that used `grep`. `head_lines` and `tail_lines` are ignored in a script, with a note saying so (`bash_exec: tail_lines is ignored in a script; pipe to tail`); `grep` on `bash_wait` and `bash_status` still applies.
- `-o json` prints `execution_id`, `status` (`exited`, `timeout` or `interrupted`), `exit_code`, `stdout`, `stderr`, `notes`, `stdout_log_path` and `stderr_log_path`.
- A script gets no history diff of what the command changed, in either output format. The snapshot is still taken, so `rollback_file` works as it does after a direct call; run `git diff` in the command to see the change.

`bash_spawn`, `bash_wait` and `bash_status` report a command that has finished the same way. A command that has not finished prints one line of JSON (`execution_id`, `status`, `notes`, `running_for_secs`, `stopped_by` when a new message or your run deadline ended the wait, and the two log paths) and exits 0, with a note on stderr when it is still running. `bash_spawn` with no `wait` prints that line with status `spawned`.

`bash_read_exec_log` prints the selected log lines as plain text, with no line-number prefix. Pass `--line-numbers` to get `N: line` rows. A direct call numbers lines unless you pass `line_numbers: false`. Notes about paging and truncation go to stderr. A forward read that stops before the last matching line, at the 200-line default or at `max_output_bytes`, ends with `N more lines. Use --offset K to continue`, where K is the line after the last one printed; a `tail` read has no such note. `-o json` adds `start_line`, `end_line` and `total_lines`.

`bash_terminate` and `bash_rollback_file` keep their usual output. Command templates with `protocol: jsonl` keep their usual output too, because their stderr is the result.

The shell form applies when the tool server is native, behind `harnx-mcp-bridge` with `--mcp-stdio`, or in the k8s sandbox. A bridged result over the bridge's 128 KiB limit is an error that says how to page.

## Shell command templates

Command templates allow defining custom fixed shell command shapes in YAML files. Each template registers as a distinct MCP tool with a strongly typed input schema. Instead of giving an agent arbitrary shell execution privileges, templates restrict execution to a fixed script where agent-provided parameters are validated against a schema and passed to the script as environment variables.

### Discovery and precedence

Command templates are discovered automatically from `$HARNX_PACKAGE_DIR/bash_tools/*.yaml`. You can also specify templates using CLI flags:

- `--tool <PATH>`: Load a single YAML template file (repeatable).
- `--tools-dir <PATH>`: Load all `.yaml` template files from a directory (repeatable).

Precedence rules:
- Explicit CLI sources (`--tool` and `--tools-dir`) take precedence over auto-discovered package templates on name collisions.
- Duplicate template names within a single source produce a hard error at startup.
- Malformed explicit template files cause a hard error at load time.
- Malformed auto-discovered template files are skipped with a warning.

### YAML format reference

Each command template file contains the following fields:

| Field | Type | Description |
| :--- | :--- | :--- |
| `name` | String (required) | Tool name. Must match `^[a-zA-Z][a-zA-Z0-9_]*$` and cannot collide with built-in tool names (`exec`, `read_exec_log`, `spawn`, `status`, `wait`, `terminate`, `rollback_file`). |
| `description` | String (optional) | Human-readable description of the tool advertised to MCP clients. |
| `parameters` | Map (optional) | Parameter definitions for the tool schema. Defaults to no parameters. |
| `env` | Map (optional) | Key-value pairs injected directly into the script environment as fixed environment variables. |
| `sandbox` | Map (optional) | Sandbox capabilities for this template. Omitting this field uses the server's default sandbox allowlist. |
| `template` | Boolean (optional) | Enable Minijinja rendering of `script` before execution. Default is `false`. |
| `on_user_message` | String (optional) | Action when a new user message arrives: `ignore` (default) or `terminate` (terminate and reap the foreground process group). Unknown values fail template loading. |
| `protocol` | String (optional) | `jsonl` makes the script's stdout a channel for live status messages and its stderr the tool's result. See [Structured protocol](#structured-protocol-protocol-jsonl). Omit it for plain output. Unknown values fail template loading. |
| `script` | String (required) | Script body to execute as bash under the sandbox. |

With `on_user_message: terminate`, native NATS calls also stop for a user message
already pending at dispatch. The foreground process group is terminated and reaped
before a normal, non-error result returns `status: interrupted`,
`stopped_by: new_message`, and partial stdout/stderr with log paths. The tool's
description automatically explains this behavior. Read the new message, then re-run
the template if still appropriate; no background execution remains to resume.
Templates with `on_user_message: ignore` (or the field omitted) and generic `exec`
calls ignore new messages. MCP stdio and k8s-sandbox proxy routes do not deliver
these signals.

`status: interrupted` doesn't prove the command failed or didn't finish its work.
The exit code isn't reported, even when the command exits just as the message
arrives, or when the shell has exited but a background child still holds its
output open.

#### Parameter definitions

Each entry under `parameters` defines an input argument:

| Field | Type | Description |
| :--- | :--- | :--- |
| `type` | String (required) | `string`, `integer`, `number`, or `boolean`. |
| `required` | Boolean (optional) | Whether the argument must be supplied by the caller. Default is `false`. |
| `description` | String (optional) | Description of the parameter shown in the MCP schema. |
| `pattern` | String (optional) | Regex pattern constraint (regex crate syntax), matched against the value's string form for any parameter type. |
| `enum` | List (optional) | List of allowed values for the parameter. |
| `min` | Number (optional) | Minimum numeric value for `integer`/`number`, or minimum character length for `string`. |
| `max` | Number (optional) | Maximum numeric value for `integer`/`number`, or maximum character length for `string`. |
| `default` | Any (optional) | Default value used when the parameter is omitted. |

Parameter names map to `UPPER_SNAKE_CASE` environment variables available to the script (e.g. `number` becomes `$NUMBER`, `some-value` becomes `$SOME_VALUE`). Parameter names must yield valid environment variable names starting with `A-Z` or `_` and cannot collide with reserved system environment variables.

#### Sandbox configuration

The `sandbox` block grants template-specific capabilities:

| Field | Type | Description |
| :--- | :--- | :--- |
| `enabled` | Boolean (optional) | Enable sandbox enforcement. Default is `true`. Setting `false` completely disables sandbox enforcement for this tool and logs a `WARN` event. Use only as a last resort. |
| `network` | Boolean (optional) | Allow network access within the sandbox. Default is `true`. Set `false` to block network access. |
| `read` | List of strings (optional) | Additive filesystem read paths. Tilde (`~`) and `$VAR` environment references are expanded at load time. |
| `write` | List of strings (optional) | Additive filesystem write paths. |
| `env` | List of strings (optional) | List of host environment variable names allowed through the sandbox's deny-by-default environment filter. |

### Environment key distinction (`env:` vs `sandbox.env:`)

Command templates contain two distinct environment keys that serve different purposes:

- **Top-level `env:`** is a map (`{KEY: value}`) that **injects** fixed, static key-value pairs into the script execution environment.
- **`sandbox.env:`** is a list (`[NAME, ...]`) that **allows through** ambient host environment variables (such as `GH_TOKEN`) that already exist in the server process environment, past the sandbox's deny-by-default environment filter.

### Canonical example

```yaml
name: gh_issue_view                 # tool name; must match ^[a-zA-Z][a-zA-Z0-9_]*$; can't collide with built-ins
description: View a GitHub issue as JSON
parameters:
  number: { type: integer, required: true, description: Issue number }
  repo:   { type: string,  required: true, pattern: "^[\\w.-]+/[\\w.-]+$" }
env: { GH_PAGER: "" }                  # top-level env: a MAP that INJECTS key=value env vars
sandbox:                               # omit entirely => server default allowlist
  read:  ["~/.config/gh"]              # additive read-path grants (~ and $VAR expanded)
  write: ["/tmp/out"]                  # additive write-path grants
  env:   ["GH_TOKEN", "GITHUB_TOKEN"]  # sandbox.env: a LIST that ALLOWS THROUGH ambient env vars
  # enabled: true  (default)           # false bypasses sandbox (logged at WARN) — last resort
  # network: true  (default)           # false blocks network
template: false                        # true renders script via minijinja first; default false
script: |
  set -euo pipefail
  gh issue view "$NUMBER" --repo "$REPO" --json title,body,comments
```

### Reserved template parameters

Command templates automatically inject four reserved parameters into their tool schema alongside author-declared parameters. Declaring any of these names under a template's `parameters` map causes loading to fail with an error naming the parameter.

Reserved parameters control process execution and output formatting. They are processed by the server and are never passed to the script environment:

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `timeout_secs` | Integer (1–86400) | 86400 | Command execution deadline in seconds. `0` is rejected. |
| `head_lines` | Integer (>= 0) | 250 | Return only the first N lines of each stream. |
| `tail_lines` | Integer (>= 0) | 750 | Return only the last N lines of each stream. |
| `max_output_bytes` | Integer (>= 0) | 131072 | Truncate output to this many bytes. Long lines are preserved within the byte cap and line window. |

#### Template output defaults

Template authors can set output defaults in an optional top-level `output:` block:

```yaml
output:
  head_lines: 25
  tail_lines: 75
  max_output_bytes: 65536
```

Each entry is optional and must be a non-negative integer. Unknown keys, negative numbers, fractional values, strings, and `null` are rejected at load time with the field name. These defaults aren't script parameters or environment bindings. Declared defaults appear in the tool's input schema.

Each field is resolved independently: a non-null caller argument overrides the template default, then the server default in the table above. An omitted or null caller argument inherits the template default. Omitting `output:` (or using `output: {}`) preserves existing server behavior.

The byte cap and head/tail line window apply to stdout and stderr independently; result metadata and formatting are outside those caps. The shipped `gh_issue_view` and `jira_view` templates keep their `max_output_bytes: 65536` defaults.

When calling a template inside the in-process `bash` script tool, the outer `bash` call has its own byte cap and line window. Increase its `max_output_bytes` or line limits separately if needed. Template defaults affect only the inner command.

#### Output truncation and long lines

Bash tools default to 250 head lines, 750 tail lines, and a 131072-byte (128 KiB) cap per stream. Per-line clipping is off by default, so long single-line JSON is returned intact when it fits the byte cap. Output exceeding the byte cap or line window is still truncated.

These defaults apply to:
- Command templates
- Foreground execution (`exec`)
- Background process output (`spawn` wait, `wait`, `status`)
- Execution log inspection (`read_exec_log`)
- The in-process `bash` script tool in `harnx-runtime`

Set `max_output_bytes`, `head_lines`, or `tail_lines` explicitly to adjust the byte cap or line window.

When template output is truncated, the hint lists only output controls:

```
[stdout truncated from X to Y. Use max_output_bytes, head_lines, or tail_lines to see more]
```

Template hints do not mention `read_exec_log` because template tools accept output controls directly, and callers may not have `read_exec_log`. Re-calling a template with larger limits re-runs its command.

### Structured protocol (`protocol: jsonl`)

A template with `protocol: jsonl` can report live status while its script runs. The TUI shows the status on the tool's row and the Web UI on its card, and links in its markdown can be opened from both. In this mode stdout and stderr do different jobs: stdout carries status messages, and stderr is the tool's result.

The script's environment gets `HARNX_TOOL_PROTOCOL=jsonl`. A script can check it to choose between status messages and plain output, so it still works when run by hand.

Each stdout line is one JSON object. A line with a `progress` object updates the status:

```json
{"progress": {"title": "PR #123 · 12 passed · 3 pending", "markdown": "[PR #123](https://github.com/o/r/pull/123): 12 passed, 3 pending"}}
```

`title` and `markdown` are optional strings, and a field left out keeps its current value. Other fields inside `progress` are ignored. A line that isn't UTF-8, isn't a JSON object, has no `progress` object, or is longer than 128 KiB is logged as a warning and skipped. A bad line never fails the call. All of stdout still goes to the execution's stdout log file, but none of it reaches the result.

stderr is the result, on success and on failure. The result keeps the usual metadata header (exit code, log paths and the rest) and contains only the stderr block, which is empty when the script wrote nothing to stderr. `total_lines` and `total_bytes` count stderr only. `head_lines`, `tail_lines`, `max_output_bytes` and the template's `output:` defaults apply to stderr, and the tool's schema describes them that way. Exit codes, timeouts and `stopped_by: new_message` work as they do in plain mode.

Status updates are live events and aren't saved to the session transcript, so write anything the model needs to stderr. The MCP stdio and HTTP routes have no progress channel: status lines are dropped there and the result is the same.

An older harnx that doesn't know `protocol` ignores the field and runs the template in plain mode. The script then sees no `HARNX_TOOL_PROTOCOL` and can print plain output instead.

Without a JSON tool such as `jq`, a bash script has to escape strings itself. This function escapes backslashes, quotes, newlines, carriage returns and tabs, and drops other control characters:

```bash
json_string() {
  local s=$1
  s=${s//\\/\\\\}; s=${s//\"/\\\"}
  s=${s//$'\n'/\\n}; s=${s//$'\r'/\\r}; s=${s//$'\t'/\\t}
  printf '"%s"' "$(printf '%s' "$s" | tr -d '\000-\010\013\014\016-\037')"
}
printf '{"progress":{"title":%s}}\n' "$(json_string "$title")"
```

### Threat model and security framing

- **Primary security boundary**: The fixed command shape combined with schema parameter validation forms the primary security boundary. The agent cannot alter the script body; it only provides input parameters validated against the declared types and pattern constraints. When `template: false` (the default), inputs are passed solely via environment variables and are never spliced into shell script text.
- **Sandbox layer**: The birdcage sandbox acts as defense in depth (a secondary isolation boundary).
- **Troubleshooting credentials and tools**: Network access is enabled by default (`sandbox.network: true`). If a CLI tool like `gh` fails inside the sandbox, it is typically because `GH_TOKEN` was filtered out by the deny-by-default environment rules or `~/.config/gh` was not readable. Fix this by adding `read: ["~/.config/gh"]` and `env: ["GH_TOKEN"]` under `sandbox:` — do not set `sandbox.enabled: false`.

### Safe script patterns

When writing template scripts, follow these practices:
- Always quote parameter environment variables: `"$NUMBER"`, `"$REPO"` (values may contain spaces or newlines).
- Begin scripts with `set -euo pipefail` to fail fast on errors, unset variables, or pipeline failures.
- Pass parameters after `--` double-dash separators where supported by the underlying CLI tool (e.g., `gh issue view -- "$NUMBER"`), preventing value strings starting with `-` from being parsed as command flags.
- Values passed through environment variables are literal string data and are never re-parsed by bash as code, making inputs such as `; rm -rf /` inert as long as variables are properly quoted in the script.

### Minijinja templating (`template: true`)

Setting `template: true` enables Minijinja template rendering of the script body prior to execution.
- Use templating for **structural changes** only (such as conditional flags or loops), e.g., `{% if verbose is defined %}--verbose{% endif %}`.
- Template context variables use original lowercase parameter names (e.g., `{{ repo }}`).
- If interpolating a parameter value directly into shell text inside a template body, apply the `| quote` filter (e.g., `{{ repo | quote }}`). Unquoted interpolations place raw strings into shell text.
- Keep `template: false` unless structural template logic is required.

### Executing non-bash scripts

Command templates always execute scripts using bash. To execute scripts written in Python, Node.js, or other interpreters, invoke the interpreter binary directly from the bash body:

```yaml
name: parse_manifest
parameters:
  file: { type: string, required: true }
script: |
  set -euo pipefail
  python3 -c 'import sys, json; print(json.load(open(sys.argv[1])))' "$FILE"
```

Parameter names are transformed to `UPPER_SNAKE_CASE` env vars, so avoid names
that map onto critical shell variables (`PATH`, `IFS`, `HOME`, `PWD`,
`LD_PRELOAD`, `LD_LIBRARY_PATH`, `BASH_ENV`, `ENV`, `SHELLOPTS`, `PS4`,
`BASH_XTRACEFD`, `CDPATH`, `GLOBIGNORE`, `BASHOPTS`), onto `HARNX_TOOL_PROTOCOL`,
which harnx sets itself, or the `BASH_FUNC` prefix.
Two parameters that map to the same env var name, or a parameter that collides
with a top-level `env:` key, are also rejected. A template that violates any of
these is rejected at load.
