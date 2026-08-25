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

Other options include `--tool <PATH>` (load one YAML command template; repeatable), `--tools-dir <PATH>` (load every `.yaml` command template in a directory; repeatable), `--no-sandbox`, `--sandbox-run <PATH>`, `--env`/`-e`, `--mcp-stdio`, and `--help`/`-h`.

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
| `name` | String (required) | Tool name. Must match `^[a-zA-Z][a-zA-Z0-9_]*$` and cannot collide with built-in tool names (`exec`, `read_exec_log`, `spawn`, `wait`, `terminate`, `rollback_file`). |
| `description` | String (optional) | Human-readable description of the tool advertised to MCP clients. |
| `parameters` | Map (optional) | Parameter definitions for the tool schema. Defaults to no parameters. |
| `env` | Map (optional) | Key-value pairs injected directly into the script environment as fixed environment variables. |
| `sandbox` | Map (optional) | Sandbox capabilities for this template. Omitting this field uses the server's default sandbox allowlist. |
| `template` | Boolean (optional) | Enable Minijinja rendering of `script` before execution. Default is `false`. |
| `script` | String (required) | Script body to execute as bash under the sandbox. |

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
env: { GH_PAGER: "" }               # top-level env: a MAP that INJECTS key=value env vars
sandbox:                            # omit entirely => server default allowlist
  read:  ["~/.config/gh"]           # additive read-path grants (~ and $VAR expanded)
  write: ["/tmp/out"]               # additive write-path grants
  env:   ["GH_TOKEN"]               # sandbox.env: a LIST that ALLOWS THROUGH ambient env vars
  # enabled: true  (default)        # false bypasses sandbox (logged at WARN) — last resort
  # network: true  (default)        # false blocks network
template: false                     # true renders script via minijinja first; default false
script: |
  set -euo pipefail
  gh issue view "$NUMBER" --repo "$REPO" --json title,body,comments
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
`BASH_XTRACEFD`, `CDPATH`, `GLOBIGNORE`, `BASHOPTS`) or the `BASH_FUNC` prefix.
Two parameters that map to the same env var name, or a parameter that collides
with a top-level `env:` key, are also rejected. A template that violates any of
these is rejected at load.
