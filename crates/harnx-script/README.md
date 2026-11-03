# harnx-script

Run BashKit scripts in-process with a fresh virtual filesystem and caller-registered
async commands. This crate has no dependency on harnx-runtime, NATS, or agent state.

```rust
use harnx_script::{BashKitBackend, CancellationToken, ScriptBackend, ScriptLimits, ScriptRequest};

async fn example() {
    let result = BashKitBackend
        .run(ScriptRequest {
            script: "echo '{\"value\":42}' | jq '.value'",
            limits: ScriptLimits::default(),
            commands: vec![], // Arc<dyn ScriptCommand>
            fs_mounts: vec![], // FsMount
            cancel: CancellationToken::new(),
        })
        .await;
    assert_eq!(result.stdout, "42\n");
}
```

## Execution boundary

- Each run creates a new `Bash` and `InMemoryFs`, with cwd `/tmp`. Shell variables,
  files, jobs, and the sticky BashKit cancellation flag don't cross runs.
- `set -e` is prepended to the script. Scripts may use `set +e`. The prelude counts
  against command fuel and shifts BashKit's diagnostic line numbers by one.
- Wall time defaults to 86400 seconds, overriding BashKit's 30-second default.
  `timeout_secs: 0` uses a finite 100-year limit to avoid overflowing platform
  clocks. Values above that duration are capped there too.
- Command fuel defaults to 10000. Each output stream is capped at 1 MiB. Other
  BashKit parser, memory, job, and VFS limits retain their defaults (including
  10 MB per file and 100 MB total VFS storage).
- Output truncation preserves the command's exit status and sets truncation flags
  plus `limit_hit`. Timeout reports `timed_out`/124; cancellation reports
  `cancelled`/130. Streaming capture retains emitted partial output on errors.
  Fatal command messages and errors returned by BashKit also appear in
  `ScriptRun.error`, independent of stream caps. BashKit 0.20.2 reports late
  syntax errors as ordinary stderr with exit 2; if its own stderr cap has
  discarded that diagnostic, the backend can't recover it.
- BashKit is pinned to `=0.20.2`, with only its `jq` feature enabled. HTTP, SSH,
  git, host filesystems, and Python/Monty aren't enabled. Curl/wget are also
  filtered out at construction. Builtins operate on virtual files, including
  BashKit's synthetic `/etc` and `/proc`, not host files.

## Registered commands and mounts

Implement `ScriptCommand` using `async_trait`. `run` receives arguments without
its command name, optional stdin bytes, and `ScriptCommandContext` (env, cwd,
virtual filesystem, cancellation token). Return `ScriptCommandOutput` for normal
success or failure. Return `Err(message)` only for a fatal error that must stop
the entire script, including under `set +e` or from a background job.

Commands use `&`/`wait` and pipelines through BashKit's scheduler. Command objects
are shared `Arc`s with `Send + Sync`; calls aren't serialized by this backend.
Keep admission limits and service-specific concurrency caps in the command's
implementation.

Cancellation selects the external token against script execution, sets BashKit's
atomic flag, and drops interpreter-owned job futures. The command context's token
is a child of the external token and is cancelled on completion, timeout, or
caller drop too. Prefer inline futures. A command that spawns detached work is
responsible for stopping it using this token; dropping a Rust future cannot undo
remote side effects. Completing a run never cancels its caller's token.

`FsMount` accepts a trusted virtual `FileSystem` mounted at an absolute path.
Mounted filesystem paths start at `/`. For read-only mounts, wrap the filesystem
in `ReadOnlyFs`. Caller-supplied mounts and commands are capabilities: only pass
implementations whose host access is intended. `BashKitBackend::builtin_names()`
lists the compiled-in commands, excluding caller registrations.

## Schema-derived arguments and help

`cli_map::parse_args(schema, argv, stdin) -> Result<Value, UsageError>` takes
already-tokenized shell arguments (without the command name). Flag names are
kebab-case; returned JSON keeps the schema's original property names. For example,
`filePath` becomes `--file-path`, but the result still contains `"filePath"`.

| Schema property | CLI form |
| --- | --- |
| string, number, integer | `--key value` or `--key=value` |
| boolean | `--key` / `--no-key` (no value) |
| array of scalars | repeat `--key value`; boolean items use `true` / `false` |
| object, complex array, reference, union, nullable, untyped | `--key '<json>'` |

`--json '<object>'` supplies a base object using original property names.
`--json -` (also `--json=-`) reads that object from stdin. Stdin is otherwise
ignored. Flags override the base regardless of ordering. Repeated array flags
replace a base array, then append in argument order. Last scalar flag and last
`--json` win. Overrides replace whole values, not nested keys.

Numbers use JSON numeric syntax, including decimal points and exponents. Integers
must fit signed or unsigned 64-bit storage and have no decimal point or exponent
(`1.0` and `1e3` are usage errors). Invalid types, invalid JSON, enum mismatches,
missing required fields, and unknown flags return `UsageError`; unknown flags
suggest nearby spellings. Defaults are shown in help but aren't inserted. Simple
types and enums are checked after merging; full JSON Schema validation, including
reference resolution, union branches, nested object constraints, ranges and
formats, remains the tool provider's job.

`--` ends options. No positional arguments are supported, so only a final `--` is
accepted. Values beginning with `--` must use `--key=VALUE`. Ambiguous kebab-case
names (including boolean negation collisions) require the `--json` escape hatch.
A property named `json` or with an empty flag spelling is also supplied through
`--json`.

Command wrappers call `cli_map::split_output_args(schema, argv)` **before**
`parse_args`. It returns `(tool_argv, Option<OutputFormat>)`, removing the reserved
`-o json|text` / `-o=json|text` rendering option (last wins). It stops at `--` and
respects schema value boundaries: `--path -o` keeps the literal path `-o`, while
`--enabled -o json` extracts the rendering option when `enabled` is boolean.
`--output` and `--o` aren't reserved. Calling `parse_args` directly with a rendering
option returns a usage error. Wrappers also own `<tool> --help`; the parser itself
doesn't handle help requests.

`help::synopsis(name, schema, description)` returns one line with required flags
first and the description's first sentence. `help::help(name, schema, description)`
returns NAME, SYNOPSIS, DESCRIPTION and OPTIONS sections with type, default and
enum metadata. Both use the parser's flag/type classification, including JSON
fallbacks for real MCP schemas with `$ref`, `anyOf` or nullable types.

## Tool command catalog

`commands::tool_commands(Vec<ToolSpec>, Arc<dyn InnerExecutor>)` returns the
`Vec<Arc<dyn ScriptCommand>>` passed to `ScriptBackend::run`. `ToolSpec` holds
`name`, `description` and `schema: serde_json::Value`. Names are identical to direct
tool names. Duplicate names and discovery-reserved `tools` / `help` are registration
errors. Tool names may replace interpreter builtins; schemas don't grant access to
tools not admitted by the executor.

Implement async `InnerExecutor::call(name, args, cancel) -> CallOutcome` in the
embedder. Share one executor across all commands in a script so background jobs
and pipelines use the same concurrency and call-budget limits. `CallOutcome::Ok`
contains the raw provider payload, not a serialized `ToolResult` envelope. Other
outcomes distinguish tool errors (with optional partial output), approval,
blocking, rejection and fatal failure. The crate has no runtime dependency.

Catalogs add `tools [pattern]` (sorted synopses, quoted glob filter),
`help <tool> [tool ...]`, and `<tool> --help`. Discovery never dispatches a tool or
runs its hooks. A standalone `--help` requests help; tool properties named `help`
can still be set with `--help=VALUE` or `--json`. Unknown commands use BashKit's
last-chance `CommandResolver` to return exit 127 with nearby-name suggestions.
This resolver only reports errors; it never dispatches an unknown tool.

Output defaults to JSON from MCP `structuredContent` when present, otherwise
concatenated MCP text blocks. Plain non-MCP strings are text; other raw values
are JSON. Nonempty output ends with a newline. `-o text` selects content text
instead of structured content, falling back to structured JSON if there's no
content text. `-o json` selects structured content or, without it, the provider
payload as a JSON document. It doesn't try to parse arbitrary text blocks as JSON.

MCP image, audio, resource-link and embedded-resource blocks become
`[attachment: <uri> <mime>]` markers instead of inline data. Without a URI the
marker uses `inline`; no attachment CID is fabricated. JSON mode replaces these
blocks with text-marker blocks, keeping valid JSON without base64 data. Structured
content takes precedence over content blocks, keeping pipelines valid JSON.
Canonical MCP `content` envelopes with `isError: true` produce exit 1 even when
the transport succeeded; raw business JSON `is_error` keys aren't status flags.

| Outcome | Exit | Streams |
| --- | --- | --- |
| success | 0 | rendered stdout |
| tool error | 1 | error on stderr; available partial output on stdout |
| usage error | 2 | diagnostic on stderr, no dispatch |
| approval required | 3 | `needs approval: <tool>; call it directly instead of from bash` |
| blocked | 4 | reason on stderr |
| rejected | 5 | reason on stderr |
| unknown command | 127 | reason and suggestion on stderr |
| fatal | abort run | error on stderr, even under `set +e` or in a background job |

Runtime integration uses `harnx_runtime::bash_inner::ScriptInnerExecutor::new(
Arc<InnerCallRun>, AbortSignal)`. Create the run from the real outer eval context
at invocation time and retain its Arc for the final trace. The adapter passes the
abort signal through unchanged and drops pending call futures on command-token
cancellation. It doesn't mutate the caller's abort signal or run detached tasks.
Outer provider wiring must still bridge whole-script abort/deadline and handle
remote-request cleanup when the interpreter drops a command future.

## Verify

```sh
cargo nextest run -p harnx-script
cargo nextest run -p harnx-script -E 'test(cancel_) | test(background_wait_) | test(dropping_run_) | test(timeout_reports_)' --stress-count=20
cargo clippy -p harnx-script --all-targets -- -D warnings
```
