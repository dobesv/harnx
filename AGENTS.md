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
│   │       ├── client/         # LLM provider clients
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
- **Client modules:** Each LLM provider lives in `crates/harnx/src/client/` and follows the patterns in `client/common.rs` and `client/macros.rs`.
- **Configuration:** `config.yaml` holds global settings. Clients and MCP servers use individual YAML files; agents are Markdown files with YAML front matter in `agents/`.
- **Dual license:** MIT OR Apache-2.0. Preserve license headers where present.

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

## Issue/task tracker

GitHub Issues is the issue/task tracker for this project.
