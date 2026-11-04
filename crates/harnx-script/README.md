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
- BashKit is pinned to `=0.20.2`, with `jq` and `python` enabled. HTTP, SSH,
  git and host filesystems aren't enabled. Curl/wget are also filtered out at
  construction. Builtins operate on virtual files, including BashKit's synthetic
  `/etc` and `/proc`, not host files.

## Embedded Python

`python3` and `python` run the same Monty interpreter, not host CPython. Both
accept `-c`, a VFS script filename, or code on stdin (`-` is optional). Interactive
mode and third-party imports aren't supported. Use quoted heredocs for multiline
code:

```bash
echo '1 2 3' > /tmp/numbers.txt
python3 - <<'EOF'
from pathlib import Path
numbers = Path('/tmp/numbers.txt').read_text().split()
print(sum(int(n) for n in numbers))
EOF
```

`open()` and `pathlib.Path` use the same VFS as Bash, including lazy reads from
full-CID `/attachments` paths. The attachment mount stays read-only. Monty's
stdlib is a subset; network, subprocesses and `re` aren't available. Monty 0.0.19
supports simple classes, but not inheritance or metaclasses. BashKit's older
"no classes" hint doesn't describe this version.

**Python cannot call tools.** No external-function handler is installed. Call
tool commands in Bash, save their output in `/tmp`, then process those files in
Python. Tool names, `tools` and `call_tool` aren't Python globals.

The backend enables in-process Python with
`.env("BASHKIT_ALLOW_INPROCESS_PYTHON", "1")` on each Bash builder. It never sets
process-global environment variables. Python gets no extra host capabilities.

`python_with_limits` uses the run's `timeout_secs` and
`ScriptLimits::max_python_memory_bytes` (64 MiB by default). BashKit also clamps
Python to the remaining run time. The parent deadline includes worker queue time
and reports `timed_out`/124 plus `limit_hit`, even when Monty returns an ordinary
`TimeoutError`. Cancellation reports `cancelled`/130; busy-loop cancellation is
tested on a single-thread Tokio runtime with a one-second bound.

Monty's heap and collected print output each use the per-invocation memory
ceiling. Exceeding it returns exit 1 and a `MemoryError: memory limit exceeded`
diagnostic on stderr; BashKit doesn't expose that VM exception as a typed
`limit_hit`. `set +e` can recover from this command error. These are cooperative
VM limits, not an OS-level process/RSS limit. VFS, parser and Bash intermediate
budgets are separate. Python print output is buffered until the invocation
returns, so cancellation/timeout can lose prints from an unfinished VM slice.

BashKit charges each Python invocation against a shared per-script work budget
(`max_memory / 64` units per invocation, consuming 1,048,576 units under the default
64 MiB limit against a 100M unit budget). This caps a single script run at approximately
95 Python invocations; invoking Python a 96th time aborts the script with a work units
resource limit. Rather than invoking `python` or `python3` repeatedly inside shell loops,
batch per-item processing into a single Python invocation.

### Known crash risks and isolation limits

The 64 MiB limit and 1-second timeout are cooperative VM checks, not hard OS-level
process or memory isolation guarantees. Certain Python operations allocate directly
on the host heap outside Monty's resource tracker. These allocations can exhaust host
memory or trigger an unrecoverable process abort (`SIGABRT`), taking down the entire
host worker process that runs script mode.

Tested through `harnx-script`'s `BashKitBackend` under production limits (64 MiB Python
memory ceiling, 1-second timeout, BashKit 0.20.2 / Monty 0.0.19), confirmed failure examples
include (non-exhaustive):

1. **Large JSON indent:** `json.dumps(0, indent=10**9)` attempts an untracked string
   allocation in native code and aborts the entire worker process (`SIGABRT`). An indent
   of `134217728` allocated ~159 MiB RSS, well past the 64 MiB limit.
2. **Exponential result export:** Returning a deeply shared list graph (`x = [x, x]`
   repeated 30 times as the final expression) expands to ~500 MiB RSS during result bridge
   conversion. Timeout and cancellation signals do not stop this native export in time,
   resulting in an allocation failure and `SIGABRT`.
3. **Import gate bypass:** BashKit filters `import re` with a line-prefix check. Prefixing
   the statement (for example `pass; import re`) bypasses the gate. Running
   `re.split('', 'a' * 4194304)` allocated ~302 MiB RSS and failed to stop cleanly at the
   1-second timeout.
4. **Parser recursion:** Very long left-associative expression chains (such as `1+1+...`,
   `x.x.x...`, `f()()...`, or `a[0][0]...`) cause a parser stack overflow during AST
   construction before execution limits can apply.
5. **Untracked string/bytes join and split:** Native preallocations in `str.join`, `bytes.join`,
   `str.split`, and `bytes.split` allocate directly on the host heap outside Monty's memory
   tracker and abort on host allocation failure.

No newer compatible BashKit release is known to resolve these issues.

**What is affected:** An allocation failure or abort crashes the entire host worker or
host process running script mode, not just the Python invocation or subshell.

**Configuration and guidance:** Python cannot be disabled separately in this version;
no builder- or configuration-level setting exists to toggle Monty off while script mode is active.
Use script mode with Python only in environments where a worker process crash is acceptable.
For untrusted prompts, leave `bash_tools` unset so script mode is not enabled.

See [Monty dependency audit](docs/monty-dependencies.md) for build scripts,
registry `noexec`, minimum Rust versions and licenses.

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

The interpreter runs on a Tokio blocking worker so synchronous Monty execution
can't prevent the caller from polling cancellation or its wall-clock deadline.
Cancellation sets BashKit's atomic flag (checked inside the VM) and cancels a
child token used by registered commands. Once an interpreter has started, an
explicit cancel or timeout awaits worker shutdown and drops interpreter-owned
jobs before returning. Unstarted queued work is aborted without waiting for
pool capacity. Dropping the caller future signals the same shutdown; cleanup
finishes at the next VM/async checkpoint. The command context's token is
cancelled on completion too. Prefer inline futures. A command that spawns
detached work is responsible for stopping it using this token; cancellation
cannot undo remote side effects. Completing a run never cancels its caller's
token.

`FsMount` accepts a trusted virtual `FileSystem` mounted at an absolute path.
Mounted filesystem paths start at `/`. For read-only mounts, wrap the filesystem
in `ReadOnlyFs`. Caller-supplied mounts and commands are capabilities: only pass
implementations whose host access is intended. `BashKitBackend::builtin_names()`
lists the compiled-in commands, excluding caller registrations.

## Read-only attachments

`fs_attachments::AttachmentFs::new(Arc<dyn AttachmentFetcher>).mount()` supplies
an `FsMount` at `/attachments`. Create a fresh filesystem for each script run;
`BashProvider` does this automatically. The fetcher stays runtime-owned, so this
crate has no NATS dependency. Its async `fetch(cid, max_bytes)` validates media CID
syntax, enforces the caller's access rules and bounds the download before allocating
the full body. The filesystem also checks the returned length.

Paths contain the **full media CID**, without an extra encoding layer:
`/attachments/cid:media:<encoded-agent>/<session-id>/<sha256>`. BashKit accepts the
colons and slashes. Keep the CID's existing agent encoding (`pantheon%2Fatlas`,
for example) unchanged; don't decode `%2F` into another directory. Given a media
CID in `cid`, `base64 -w0 "/attachments/$cid" | wc -c` counts its base64 bytes.
`fs_attachments::attachment_path(cid)` builds this path and checks its path shape;
the fetcher remains responsible for CID validation and authorization. Plan CIDs
aren't mounted.

- Files load on the first read or file metadata lookup. Successful bytes are
  cached for that invocation; errors aren't cached, so a later read can retry.
  Stat and `test -f` probes are metadata-only and do not download content or consume
  cache budget.
  Concurrent misses serialize to avoid duplicate downloads and unbounded pending
  buffers. When the cache is full, subsequent reads fail fast before downloading
  any content. Dropping a run drops pending fetch futures, with no detached tasks.
- Directory listings are always empty, even after successful reads. The root and
  CID parent paths are virtual directories, not a view of object-store keys.
  Scripts must already know the complete CID; globs cannot discover attachments.
- Mutations fail with `EROFS: attachment filesystem is read-only`. Copying bytes
  into `/tmp` is allowed. Missing CIDs report `attachment not found: <cid>`.
- Default limits match BashKit: 10,000,000 bytes per attachment, 100,000,000 cached
  bytes and 10,000 cached files, plus its path limits. Oversized files fail; they
  aren't truncated. When total cached bytes reach the limit, downloads fail with
  `attachment cache is full (<limit> byte limit)`. `AttachmentFs::with_limits`
  changes these limits for an embedder. This is a separate mount budget from the
  writable root VFS budget.
- Runtime reads use the same broker connection/credentials as the round's tools,
  without creating buckets. Access matches current `attachment_read`: canonical,
  explicit media CIDs may refer to another session. The CID owner determines its
  storage key, **not** a same-session ACL. The mount doesn't enumerate storage or
  add a bypass around broker permissions. If the round has no NATS connection,
  listings still work and reads report that attachment storage is unavailable.

### Inner tool media

Runtime externalizes inline images, audio and embedded resource bodies before
returning an inner result to a script. It uses the round's NATS connection and
parent agent/session identity. Uploads finish before the command returns, so the
next command can read the exact bytes through `/attachments` without waiting for
round-end session synchronization. Inner media doesn't create nested transcript
rows or automatically return an image from the outer `bash` call.

Text output contains `[attachment: cid:media:… (mime, size)] path=/attachments/<full-cid>`.
In `-o json` output, each inline media block becomes a text block with `text`,
`uri`, `mimeType`, decoded `size` and `path` fields. For an MCP image result:

```bash
path=$(screenshot -o json | jq -r '.content[0].path')
wc -c < "$path"
```

The runtime also replaces inline media in `structuredContent`, recoverable partial
results and JSON text mirrors of structured results. Output format selection
can't bypass externalization. Existing non-inline links aren't downloaded, and
ordinary non-media values keep their shape. Scripts can still print base64
explicitly with `base64 -w0 "$path"`; externalization removes automatic inline
payloads, not deliberately generated script output.

Malformed media and files over the 10,000,000-byte attachment limit are inner
errors. Storage failures (including missing NATS or a missing session identity) stop
the script, even under `set +e`; they never fall back to returning inline bytes.
Media upload remains inside the inner-call concurrency cap. Cancellation drops
pending upload futures, but it can't undo an upload already accepted by NATS.

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

## Generated `bash` description

`description::bash_tool_description(tools, concurrent_cap, call_cap)` builds the
model-facing guide from the resolved `ToolSpec` catalog. Builtins come from
`BashKitBackend::builtin_names` (already filtered to exclude `curl`/`wget`);
sorted tool synopses use the same generator as `tools` and `--help`. Runtime
supplies admission caps; execution and filesystem defaults come from their limit
types. The guide includes quoting patterns, Monty restrictions and one pipeline
using a compatible file-reader tool, or an explicitly labeled generic example.

The PNG attachment idiom appears only for an `attachment_create`-suffixed tool
whose string flags accept `mime-type`, `encoding=base64` and `content`, with no
other required arguments. Tools without an `encoding` property don't advertise
binary creation. Snapshot tests log a rough characters/4 token estimate; execution
tests run the quoting patterns and generated commands against BashKit.

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
