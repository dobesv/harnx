# AGENTS.md — Harnx

## Project Overview

Harnx is a modular command-line LLM agent harness written in **Rust**. It lets users build custom agents from the ground up with full control over prompts, tools, models, and sub-agents. It integrates with 20+ LLM providers (OpenAI, Claude, Gemini, Ollama, Bedrock, etc.) and supports MCP (Model Context Protocol) servers.

## Technology Stack

- **Language:** Rust (edition 2021, toolchain pinned in `rust-toolchain.toml` — rustup and CI both read this file automatically)
- **Async runtime:** Tokio (multi-threaded)
- **Test Runner:** Nextest
- **HTTP client:** reqwest (rustls-tls)
- **CLI framework:** clap (derive)
- **Serialization:** serde + serde_json + serde_yaml
- **TUI:** ratatui + crossterm
- **RAG:** hnsw_rs + bm25
- **MCP SDK:** rmcp
- **CI:** GitHub Actions (see `.github/workflows/ci.yaml`)
- **Release tooling:** [knope](https://knope.tech) (see `knope.toml`)
- **Dependency management:** Renovate (see `renovate.json`)

## Repository Layout

```
├── Cargo.toml                  # [workspace] manifest — shared dep versions live here
├── crates/
│   ├── harnx/                  # Main CLI and TUI crate
│   │   ├── Cargo.toml
│   │   ├── assets/             # Bundled assets (syntax/theme .bin, HTML playgrounds)
│   │   ├── models.yaml         # Model catalog (providers, pricing, capabilities)
│   │   ├── tests/              # Integration tests
│   │   └── src/
│   │       ├── main.rs         # Entry point for the `harnx` binary
│   │       ├── lib.rs          # Library root — re-exports modules
│   │       ├── cli.rs          # CLI argument parsing (clap)
│   │       ├── serve.rs        # HTTP server mode
│   │       ├── tool.rs         # Built-in tool definitions
│   │       ├── mcp_safety.rs   # MCP tool safety classification
│   │       ├── config/         # Configuration, agent/session management
│   │       ├── render/         # Markdown + streaming output
│   │       ├── tui/            # Interactive TUI (ratatui)
│   │       ├── commands.rs     # Dot-command handlers (.help, .model, .session, …)
│   │       ├── rag/            # RAG pipeline
│   │       ├── mcp/            # MCP client/server integration
│   │       ├── hooks/          # Event hook system
│   │       ├── utils/          # Shared utilities
│   │       └── bin/            # Bins that share harnx library code (mcp-bash, mcp-fs)
│   ├── harnx-plans-tools/        # MCP server: file-based plan and todo management (standalone crate)
│   ├── harnx-mcp-time/         # MCP server: time/timezone utilities (standalone crate)
│   └── harnx-test-bins/        # Internal dev/test binaries (publish = false)
├── example_config/             # Example user configuration
├── docs/                       # User-facing documentation
├── scripts/                    # Shell completions and shell-integration scripts
├── Argcfile.sh                 # Developer helper commands (argc-based; install moved to xtask)
├── xtask/                      # Rust task runner (`cargo xtask install`) for local automation
├── .changeset/                 # Changeset files for release notes
├── knope.toml                  # Release automation config
├── renovate.json               # Dependency update bot config
└── .github/workflows/          # CI (ci.yaml) and release (release.yaml) workflows
```

## Verifying Changes

You MUST run the full verification pipeline before committing:

```sh
cargo build --workspace                                       # Compile the project
cargo fmt --all                                               # Auto-format code (rustup uses rust-toolchain.toml version — matches CI)
cargo clippy --workspace --all-targets -- -D warnings         # Lint — treat warnings as errors
cargo nextest run --workspace --stress-count=5                # Run all tests, repeat several times to catch flaky tests
cs delta origin/HEAD                                          # Run CodeScene code quality analysis on current branch changes                                          
```

**Use `cargo nextest`, never `cargo test`.** Tests rely on nextest's per-test
process isolation; `cargo test` shares one process and produces spurious
failures. The tmux/interrupt e2e tests guard against this and will panic with a
redirect message if run under `cargo test` (via `harnx_core::require_nextest()`).

FD-redirection tests (e.g., `cli_event_sink.rs` `final_usage_is_standalone`)
use `dup2` to capture stdout/stderr. Under `cargo test`, libtest's own capture
intercepts writes before `dup2` sees them, yielding empty buffers and misleading
test failures.

**Do not skip any of these steps or you WILL miss problems**
**Do not ignore clippy warnings.** CI sets `RUSTFLAGS=--deny warnings` and runs `cargo clippy -- -D warnings`, so any warning will fail the build.
**CodeScene Health scores MUST NOT decrease as part of the change, only increase**

### Broker-backed tests and wall-clock margins

Tests that spawn a `nats-server` run in the `broker-e2e` / `heavy-e2e` groups
(`.config/nextest.toml`). On a contended GitHub runner that whole block has been
measured running 6 to 40 times its idle cost, comparing runs whose diffs did not
touch it: `cancellation::hierarchy::direct_child_cancellation_stops_only_its_worker_subtree`
went 0.53s to 21.2s, and the ubuntu job 184s to 483s. Tests whose duration is a
fixed sleep stayed flat, so the cause is starvation of real work rather than
clock skew. The trigger is not understood.

Size deadlines in these tests against the degraded cost, not the idle one. A
per-call timeout with a few hundred milliseconds of slack, or a turn backstop
set near the idle duration, reports a slow runner as a broken turn; those two
shapes caused 23 of 31 CI failures in one sample of 100 runs. The `ci` profile's
`slow-timeout = { period = "60s", terminate-after = 4 }` is what catches a real
hang, so an in-test backstop only has to beat 240s and say something more useful
than a SIGKILL would.

When one of these tests fails, check a passing run's timings for the same block
before blaming the test. If the whole block is slow, the margin is the bug.

### Web/Frontend Verification

**Run all web/frontend commands from `web/`, never the repo root.** The root has no
`package.json`, so corepack cannot resolve the pnpm version and attempts to download
into a read-only cache, failing with `EROFS: read-only file system`. The `web/`
directory has `web/package.json` with the pnpm version pinned in `packageManager`.

```sh
cd web
pnpm exec tsc -b                                    # Typecheck (NOT tsc --noEmit — root tsconfig has files: [] so it always exits 0)
pnpm exec oxlint                                    # Lint
pnpm exec vitest run                                # Unit tests
pnpm test:e2e                                       # Playwright end-to-end tests
```

**Use `tsc -b`, not `tsc --noEmit`.** The root tsconfig sets `files: []` so
`--noEmit` is hollow and always exits 0; `-b` builds the actual project references.

## Commit Conventions

This project uses [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <description>
```

Common types:
- `feat` — New feature
- `fix` — Bug fix
- `docs` — Documentation only
- `style` — Formatting, whitespace (no logic changes)
- `refactor` — Code restructuring (no new features or fixes)
- `perf` — Performance improvement
- `test` — Adding or updating tests
- `chore` — Build, tooling, dependency updates

Examples from the project history:
```
feat: add harnx-plans-tools as a file-based plan and todo management MCP server
chore(deps): update rust crate syntect to v5.3.0
```

## Changeset Files

When making a user-visible change, create a changeset file in `.changeset/`:

```markdown
---
harnx: minor
---
Brief description of the change.
```

The YAML front matter specifies the version bump: `patch`, `minor`, or `major`.

The key on the left must be one of the three packages knope versions:

- `harnx` — the entire Rust workspace. All `harnx-*` crates share one version
  (`version.workspace = true`), so use `harnx` even for a change scoped to a
  single crate like `harnx-proxy-auth` or `harnx-core`.
- `pantheon` — the `packages/pantheon` agent package.
- `coding` — the `packages/coding` agent package.

Individual crate names are **not** valid keys; `knope release` will error on
them.

## GitHub Actions workflows that open pull requests

Open PRs with a token minted from the `harnx-release-bot` GitHub App
(`actions/create-github-app-token@v3` with `vars.RELEASE_BOT_APP_ID` and
`secrets.RELEASE_BOT_PRIVATE_KEY`), never the workflow's own `GITHUB_TOKEN`.
A PR opened with `GITHUB_TOKEN` gets its `pull_request` workflow runs parked as
"action required", so CI never starts and Mergify has nothing to queue until a
maintainer approves the run by hand. App-authored PRs run CI immediately.
`.github/workflows/update-models.yml` is the reference.

When attributing the bot's commits, its noreply address takes the bot *user*
id (`gh api /users/<app-slug>[bot] --jq .id`), not the installation id the
token action exposes.

## Key Patterns

- **Error handling:** Use `anyhow::Result` / `anyhow::bail!` throughout.
- **Async:** All I/O is async via Tokio. Use `async fn` and `.await`.
- **Client modules:** Provider clients live in `crates/harnx-client/src/` and follow the patterns in `macros.rs`. Config structs live in `crates/harnx-core/src/provider_config/`.
- **Configuration:** `config.yaml` holds global settings. Clients and MCP servers use individual YAML files; agents are Markdown files with YAML front matter in `agents/`.
- **Dual license:** MIT OR Apache-2.0. Preserve license headers where present.


### Reasoning-signature compatibility across providers (issue #1804)

Each provider has its own opaque reasoning-signature format:
- **Anthropic**: `thinking` block `signature` (also used by Bedrock Claude)
- **Gemini**: `functionCall` part `thoughtSignature`
- **OpenAI**: `reasoning` item `encrypted_content` (shared by codex)

These formats are **not interchangeable**. Replaying a signature to an incompatible provider
causes HTTP 400, which halts fallback (400 is non-retryable). The fix added
`reasoning_provenance: Option<ReasoningProvenance>` to `ToolCall`
(`harnx-core/src/tool.rs`), tagging each signature with its producing protocol and model.

Compatibility rules (verified in `ToolCall::compatible_signature`):
- **OpenAI**: fail-closed on model identity — `encrypted_content` is bound to both org and model.
  Same-protocol, different-model → incompatible.
- **Anthropic/Gemini**: protocol must match; model binding is not enforced.

Import handling when signature is incompatible or unknown (legacy sessions lack provenance):
- **Gemini**: first `functionCall` of each step must carry `thoughtSignature`. Use placeholder
  `"skip_thought_signature_validator"` for imported/unknown history (per Google docs).
- **Anthropic**: omit the `thinking` block entirely. Missing signature → 400.
- **OpenAI**: omit the `reasoning` input item. Safe because harnx uses `function_call` with
  `call_id`, not OpenAI-internal `id`/`fc_` which would trigger reasoning-pairing validation.

Architecture: provenance-aware normalization lives in each provider's `build_body` (request-local,
never mutates stored history). Each builder calls
`tool_call.compatible_signature(dest_protocol, dest_model)` and applies destination handling.

When modifying provider client code: tag provenance at capture (streaming/non-streaming extraction), check compatibility before replay, and allocate correlation IDs for imported anonymous tool calls (Gemini doesn't return IDs; use `ToolCallIdAllocator` in each `build_body`). Do NOT add a shared pre-pass — each provider knows its own
protocol and import-handling rules.


### Adding a Provider Client

Wire a new provider client via `register_client!` in `crates/harnx-client/src/lib.rs`:

```rust
register_client!(
    (myprovider, "myprovider", MyProviderConfig, MyProviderClient),
    // ...
);
```

This macro expands to the module declaration, config enum variant, and client registry. Then:

1. Add a config struct to `crates/harnx-core/src/provider_config/myprovider.rs` and export it in that dir's `mod.rs`.
2. Add `ClientConfig::MyProviderConfig(_)` match arms in `lib.rs` (`effective_name`/`set_name`/`set_package`) and `crates/harnx-runtime/src/config/patches_split.rs` (`apply_client_patch`).
3. Implement `Client`:
   - Sync auth (API key): use `impl_client_trait!` macro (see `cohere.rs` for example).
   - Async per-request auth (OAuth token refresh): write a manual `impl Client` like `vertexai.rs` or `codex.rs`, running token prep at the top of each `*_inner` method.
4. For Responses API variants, key `model.endpoint()` to `"responses"` to reuse `openai_responses.rs` helpers.

Env-var field access uses `config_get_fn!` — the macro generates `${STEM}_${FIELD}` lookup where STEM is the client filename (e.g. `myprovider_api_key` → `MYPROVIDER_API_KEY`).

### Tool-call argument parsing

When a provider client parses tool-call arguments from LLM output, use this pattern:

```rust
let arguments: Value = if arguments_str.is_empty() {
    json!({})
} else {
    serde_json::from_str(arguments_str)
        .with_context(|| format!("Tool call '{name}' have non-JSON arguments '{arguments_str}'"))?
};
```

Empty string maps to `{}` (API omits arguments for no-arg calls). Non-empty malformed JSON
propagates as an error with context naming the tool and echoing the raw argument string.
This convention is consolidated across all provider parsers (`openai.rs`, `openai_responses.rs`,
`bedrock.rs`, `claude.rs`, `cohere.rs`).

### Tool result templates and undefined behavior

Tool display templates are rendered by `make_template_env()` in
`crates/harnx-core/src/tool.rs`, which MUST use `UndefinedBehavior::Chainable`
(not `Lenient`). MiniJinja's `Lenient` mode tolerates printing an undefined
value, but it still raises "undefined value" when accessing an attribute or
index of an undefined intermediate — `default()` filters cannot rescue this.

The canonical result template `{{ result.content[0].text | default('') }}`
walks into `result.content`, which is absent on recoverable-error results
(`{"is_error": true, "error": ...}` — no `content` field). Under `Lenient`,
indexing into undefined raises before `default('')` applies (#1537).

When writing result templates:
- be null-safe for the error-result shape (`result.content` may be absent)
- `| default(...)` only works because Chainable makes missing intermediate
  paths evaluate to undefined; syntax errors and unknown filters still error

### Native toolset error mapping

Native `Toolset` implementations' `map_result` must map **all** `ErrorData` from handlers (both
`internal_error` and `invalid_params`) to `ToolInvokeError::Recoverable`. Reserve `Fatal` for:

- result-serialization failure in the `Ok` branch (`serde_json::to_value`)
- true transport/lifecycle death (e.g. MCP bridge `TransportClosed`)

**Why:** `Fatal` aborts the agent turn; `Recoverable` surfaces as `{"is_error": true, ...}` so the
session continues and the agent can retry. The canonical pattern is
`crates/harnx-fs-tools/src/toolset.rs:59-66`. When adding a native toolset, do not special-case
`ErrorCode::INTERNAL_ERROR` to `Fatal`.

### MCP ServerHandler::call_tool error mapping

MCP server implementations (`ServerHandler::call_tool`) must return recoverable failures as
`Ok(CallToolResult::error(vec![ContentBlock::text(msg)]))` (`is_error: Some(true)`). This applies to:

- argument deserialization errors (`parse_arguments`) and input validation failures
- domain execution failures (missing resources, file I/O errors, failed text replacements, rate limits)

Reserve `Err(ErrorData)` exclusively for:

- unknown tool names (`ErrorData::invalid_params("unknown tool: ...")`)
- malformed envelopes rejected before handler execution
- broken transport, lifecycle, or session state

Do not return `method_not_found` (-32601) for unknown tool names in `tools/call`. That code is
reserved for unknown JSON-RPC methods. Unknown tool requests must return `invalid_params` (-32602).

Per SEP-1303 and the MCP specification, client agents use `is_error: true` results to see error text
and self-correct. Returning JSON-RPC error frames for domain or argument errors causes client SDKs to
abort the session. See `crates/harnx-mcp-plans-core` for the reference implementation.

### Session log entries and transcript protocol

Local broker failover keeps the authenticated endpoint stable and runs election
through `nats_local_server::LocalBroker` even while a frontend is idle. Create
production connections with `harnx_nats_common::connect::NatsEndpoint`; use the
operation-specific recovery helpers documented in `docs/nats-ha.md` under
“Recovery contract and shared implementation”. Retrying a handler whose side
effects are unknown is unsafe. Failover tests must retain existing clients and
in-flight work while removing the owner, rather than testing only fresh clients.

`SessionLogEntry` (`harnx-core/src/session.rs`) and `SessionEvent` (`harnx-core/src/event.rs`)
are different types with different change-cost:

- **`SessionLogEntry`** — durable transcript entries persisted to NATS. Adding a variant is a
  **transcript-protocol change**; canonical replay hard-rejects `Unknown`, so older workers
  cannot read transcripts with new variants. Deploy readers before writers. Precedents:
  `TurnEnd` (#1490), `Error` (#1545), `SubAgentStarted` (#1604).

- **`SessionEvent`** — advisory events emitted to live subscribers, not persisted. Adding an
  optional field (with `#[serde(default, skip_serializing_if)]`) is a safe additive change.
  Used when a live client needs data that isn't in the durable entry (e.g. `after_seq` field
  in `HandoffCommitted` added in #1803).

When extending handoff or session metadata, check which type carries the data. The durable
entry is the source of truth; the advisory event is a convenience for clients that haven't
reloaded the transcript.

Required match-site updates (3 compile-time exhaustive matches):
- `config/session.rs` — reconstruction into `Session.messages`
- `nats_session.rs` — `render_log_entry_to_sink` (usually a no-op arm with comment)
- `session_history.rs` — `entry_type` and `entry_searchable_text`

Wildcard matches elsewhere (`session_reconstruct.rs`, fence helpers, etc.) compile without
changes but should be audited for correctness.

Mid-tool entries (arriving between `ToolCalls` and `ToolResults`) must be queued in
`messages_queued_during_tool` during reconstruction so tool_use→tool_result adjacency is
preserved. See `SubAgentStarted` handling in `config/session.rs` for the pattern.

Session identity is `(agent, local_id)` within a cluster. Use `Session::storage_key()`,
`NatsSession::storage_key()`, or `SessionInitializer::session_key(local_id)` for all
broker storage, control, execution, and parent references. Internal protocol fields
named `session_id` carry this key; public metadata, tool results, hooks, and
confirmation requests retain the local ID.
See `docs/nats-ha.md` under “Session identity”. Do not pass a local ID to by-key APIs.

Client/control code can append to another session's log via
`NatsSessionLog::new(jetstream, storage_key)`. Worker/tool output must instead go
through the session log backend while holding the lease, so the append is fenced
on the tail the turn last observed and a `Cancel` that landed meanwhile stops it.
A sub-agent start in the parent log is written through the invoking tool's
handle on that same lease.

The worker appends the durable `HandoffCommitted` entry **before** emitting the advisory
`SessionEvent::HandoffCommitted` (see `agent_loop.rs:979-1001`). This guarantees a live
handoff's sequence is strictly greater than any attach boundary captured before the commit,
enabling clients to gate navigation on `after_seq > attached_seq`.

Worker appends go through `FencedSessionLogSink`, which carries the lease fence and
the expected tail. Stream-tail CAS is the authority: JetStream's finite dedup window
alone is not enough, so keep the same message ID and expected-last-sequence across a
retry and treat a conflict as a decision to re-read, never as a reason to append
again. A `Cancel` is an ordinary log entry any holder of the session can write,
including frontends that hold no lease. See `docs/nats-ha.md` under "Interruption".

### Interrupt acceptance

Interruption is established by one durable `Cancel` entry in the session log and
by nothing else. Acceptance is that append landing, not a tool acknowledgement,
a lease disappearing or a process exiting. TUI, one-shot and followers return on
it; don't reintroduce a wait for tools to die before the editor comes back.

Tool protocol v5 carries `CancelAcceptance::{Accepted, AlreadyFinished, Rejected,
Unknown}` and nothing else. There is no cleanup status to report and no physical
shutdown to confirm — a cancel is best effort and its acknowledgement is for
logging. Cancels are idempotent, so resending one is always allowed and is how
wind-up and resume reach a call whose original invocation is gone. Don't add a
parallel cleanup flag, and don't read missing metadata as proof anything stopped.

Resource owners keep cleanup handles outside turn/reply futures. A dropped
`JoinHandle` detaches its task, and started `spawn_blocking` work can't be aborted.
MCP cancellation closes only the request waiter; never restart shared
infrastructure to cancel a call. See `docs/nats-ha.md` under "Interruption".

Build the workspace before cross-process tests after changing tool or hook wire
types. Those tests launch workspace sidecars as well as linked test code; a stale
hook or tool binary can fail decoding even when a per-crate build succeeds.

### TUI transcript items are TUI-local

`TranscriptItem` (`harnx-tui/src/types.rs`) derives only `Clone + Debug` — it is **not** serialized to
NATS. Adding a field or variant is a local TUI change, not a transcript-protocol change. Contrast
with `SessionLogEntry` variants (previous section), which are protocol-versioned.

### Spawning long-lived child processes

Spawn any child that must not outlive harnx through
`harnx_core::child_process::ChildProcessManager`, not `Command::spawn` directly.

`kill_on_drop(true)` alone is not process-exit cleanup. It fires only when the
`Child` value is dropped, so a handle reachable from a `static` — a process-wide
registry, a cache, a `OnceLock` — is never killed at all: Rust does not run
destructors on statics at process exit, and the child reparents to PID 1. The
llama-server registry in `crates/harnx-client/src/llama_server/process.rs` relied
on this and stranded a live `llama-server` on every exit, which accumulated into
thousands of orphaned mock servers across test runs.

`ChildProcessManager` has the kernel enforce it instead (`setpgid` +
`PR_SET_PDEATHSIG`), which also covers panic, abort, and SIGKILL. It spawns from
one stable OS thread because Linux binds `PR_SET_PDEATHSIG` to the *thread* that
forked: spawning straight from a Tokio worker lets `block_in_place` hand that
worker to the blocking pool, whose idle threads retire and take healthy children
down with them.

`PR_SET_PDEATHSIG` is Linux-only and has no portable equivalent, so on macOS and
Windows a child held by a `static` still outlives its parent — the manager only
puts it in its own process group there. Gate tests that assert parent-death on
`target_os = "linux"`, as `harnx-core`'s own child-process tests do. Anything
that must be cleaned up off Linux needs an explicit shutdown path instead.

Keep `kill_on_drop(true)` as well — it retires the child promptly when its
manager is dropped while the process keeps running.

Test helpers that spawn a broker need the same guarantee for a narrower reason:
a failing assertion unwinds past the helper's own `kill`/`wait`, and
`std::process::Child` does not reap on drop. The stranded `nats-server` then
competes with every later broker test in that nextest run, and `retries = 3`
turns one flake into several. `spawn_test_nats` returns `TestNatsServer`
(`crates/harnx-runtime/src/nats_worker/tests.rs`) and the integration harness
returns `NatsServerHandle` (`crates/harnx-runtime/tests/common/mod.rs`); both
reap in `Drop`. Keep new broker helpers in that shape.

## CLI Flag Constraints

The root `Cli.file: Vec<String>` has `#[clap(short, long, global = true, hide = true)]` at
`crates/harnx/src/cli.rs:42`. This reserves `-f` globally for `--file`. New format flags (e.g.
`--format`) MUST NOT define a short form — use long-only. See `sessionFormat` in
`harnx-runtime/src/config/session_format.rs` and the `--format` flag in `harnx/src/cli.rs`
for the precedent (PR #1448).

## Durable vs Advisory Event Contract

`SessionLogEntry` (`harnx-core/src/session.rs`) is durable and persisted to NATS JetStream.
`AgentEvent` (`harnx-core/src/event.rs`) is advisory and emitted to the lossy fan-out subject
`sessions.{id}.events`. The durable entry is authoritative; advisories are best-effort previews.

### Follow mode MUST emit durable entries only

`SessionEventStream::attach()` (`nats_event_sink.rs:386`) subscribes to advisories first, then
loads durable history. Live clients call `refresh_history()` on wake to poll for newly committed
entries — this is REQUIRED because some durable entries (e.g. `TurnEnd`) have NO advisory event.

When implementing `--follow` or live-tail rendering (CLI or TUI):

1. Replay `stream.history()` for initial output.
2. In the follow loop, `tokio::select!{ next() | timeout | ctrl_c() }`.
3. On wake, record `old_len = stream.history().len()`.
4. Call `stream.refresh_history().await` and emit `history()[old_len..]` as `SessionLogEntry`.
5. NEVER serialize `AdvisoryEnvelope.event` directly — it's a preview, not durable state.

`--follow` is read-only observation; it does not interrupt or cancel the running session.

### Entry rendering for text output

`replay_entries_to_sink()` (`nats_session.rs:1473`) renders `SessionLogEntry` tuples through a
frontend's `AgentEventSink`. Callers must first apply log mutations (edits/rewinds) if they need
an effective snapshot — the helper renders entries in the supplied order without resolution.
Control entries (`TurnEnd`, `HandoffCommitted`, `HitlApproval*`, `SubAgentStarted`) are silent
in text rendering because their state hydrates separately from human transcript output.

### Metadata rendering requires session reconstruction

Text-format session metadata (`.info session` or `harnx info session`) uses `session::render()`
(`config/session.rs:500`) which requires a reconstructed `Session` with model resolution and
token counts. The helper `load_session_for_render()` (`session_format.rs:88`) loads KV metadata,
resolves the model via `overrides.model` or the named agent's config, reconstructs transcript-
derived fields (`turns`, `tokens`), and calls `update_tokens()`. Do NOT pass the raw
`SessionMetadata` KV record to `session::render()` — it lacks resolved model and transcript state.

## Usage Accounting Semantics

`ModelEvent::Final.usage` (`harnx-core/src/event.rs:66-71`) is a **display-only per-turn total** that
sums every model completion in that turn's tool loop. Session cumulative totals and status-bar
metrics use a **separate mechanism**: `record_completion_usage` in `config/mod.rs:1082-1085` writes to
`Session.completion_usage` per model call. These mechanisms are independent. Anyone modifying usage
display must keep them separate or they'll double-count.

### TUI printable-character keybindings with SHIFT-tolerant matching

Crossterm may report shifted printable characters (`<`, `>`, `G`) with `KeyModifiers::SHIFT` set on
some terminals. Binding these characters with strict `KeyModifiers::NONE` causes the match arm to
silently never fire.

Pattern for shift-sensitive char bindings:
```rust
(KeyCode::Char('g' | '<') | KeyCode::Home, KeyModifiers::NONE | KeyModifiers::SHIFT) => { ... }
```

Accept `NONE | SHIFT` on char arms (not CONTROL/ALT combinations). Home/End keycodes don't need
SHIFT tolerance — they're not char keys. See AgentPicker in `input.rs` for the `||` guard variant,
and jump-key handlers in `detail_view.rs`/`input.rs`/`subagent_sessions.rs` for the or-pattern form.

## Issue/task tracker

### Session Unread State

Session-level unread state tracks sessions requiring user attention. Key endpoints:

- **TUI**: In the session picker, press `'u'` or `'U'` to toggle unread on the selected session.
- **Web**: SSE `/v1/agents/{agent}/sessions/{session}/events` emits `event: read-updated` when read-state changes, triggering session list refresh.
- **JSON-RPC**: `session/mark_read` and `session/mark_unread` methods control state.

Implementation details in [`docs/nats-ha.md#session-unread-state`](docs/nats-ha.md#session-unread-state).


GitHub Issues is the issue/task tracker for this project.
