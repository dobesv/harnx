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

**Do not skip any of these steps or you WILL miss problems**
**Do not ignore clippy warnings.** CI sets `RUSTFLAGS=--deny warnings` and runs `cargo clippy -- -D warnings`, so any warning will fail the build.
**CodeScene Health scores MUST NOT decrease as part of the change, only increase**

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

## Key Patterns

- **Error handling:** Use `anyhow::Result` / `anyhow::bail!` throughout.
- **Async:** All I/O is async via Tokio. Use `async fn` and `.await`.
- **Client modules:** Provider clients live in `crates/harnx-client/src/` and follow the patterns in `macros.rs`. Config structs live in `crates/harnx-core/src/provider_config/`.
- **Configuration:** `config.yaml` holds global settings. Clients and MCP servers use individual YAML files; agents are Markdown files with YAML front matter in `agents/`.
- **Dual license:** MIT OR Apache-2.0. Preserve license headers where present.


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

### Session log entries and transcript protocol

Adding a `SessionLogEntry` variant in `harnx-core/src/session.rs` is a **transcript-protocol
change**. Canonical NATS replay hard-rejects `Unknown`, so older workers cannot read
transcripts containing new variants. Deploy readers before writers in multi-instance
clusters. Precedents: `TurnEnd` (#1490), `Error` (#1545), `SubAgentStarted` (#1604).

Required match-site updates (3 compile-time exhaustive matches):
- `config/session.rs` — reconstruction into `Session.messages`
- `nats_session.rs` — `render_log_entry_to_sink` (usually a no-op arm with comment)
- `session_history.rs` — `entry_type` and `entry_searchable_text`

Wildcard matches elsewhere (`session_reconstruct.rs`, fence helpers, etc.) compile without
changes but should be audited for correctness.

Mid-tool entries (arriving between `ToolCalls` and `ToolResults`) must be queued in
`messages_queued_during_tool` during reconstruction so tool_use→tool_result adjacency is
preserved. See `SubAgentStarted` handling in `config/session.rs` for the pattern.

Append to another session's log via `NatsSessionLog::new(jetstream, session_id)` with no
`fence_token`. Used when a tool/client needs durable state visible to a session it doesn't
hold the lease for (e.g. sub-agent start entries in parent log).


## Issue/task tracker

GitHub Issues is the issue/task tracker for this project.
