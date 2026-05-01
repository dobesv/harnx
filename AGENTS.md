# AGENTS.md — Harnx

## Project Overview

Harnx is a modular command-line LLM agent harness written in **Rust**. It lets users build custom agents from the ground up with full control over prompts, tools, models, and sub-agents. It integrates with 20+ LLM providers (OpenAI, Claude, Gemini, Ollama, Bedrock, etc.) and supports MCP (Model Context Protocol) and ACP (Agent Client Protocol) servers.

## Technology Stack

- **Language:** Rust (edition 2021, toolchain pinned in `rust-toolchain.toml` — rustup and CI both read this file automatically)
- **Async runtime:** Tokio (multi-threaded)
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
│   ├── harnx/                  # Main crate: CLI, TUI, serve, ACP server, client, config, …
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
│   │       ├── acp/            # ACP client/server integration
│   │       ├── hooks/          # Event hook system
│   │       ├── utils/          # Shared utilities
│   │       └── bin/            # Bins that share harnx library code (mcp-bash, mcp-fs)
│   ├── harnx-mcp-todo/         # MCP server: file-based todo list (standalone crate)
│   ├── harnx-mcp-time/         # MCP server: time/timezone utilities (standalone crate)
│   └── harnx-test-bins/        # Internal dev/test binaries: mock-llm, acp-test, repro249, test-ratatui (publish = false)
├── example_config/             # Example user configuration
├── docs/                       # User-facing documentation
├── scripts/                    # Shell completions and shell-integration scripts
├── Argcfile.sh                 # Developer task runner (argc-based)
├── .changesets/                # Changeset files for release notes
├── knope.toml                  # Release automation config
├── renovate.json               # Dependency update bot config
└── .github/workflows/          # CI (ci.yaml) and release (release.yaml) workflows
```

## Verifying Changes

Run the full verification pipeline before committing:

```sh
cargo build --workspace                                       # Compile the project
cargo fmt --all                                               # Auto-format code (rustup uses rust-toolchain.toml version — matches CI)
cargo clippy --workspace --all-targets -- -D warnings         # Lint — treat warnings as errors
cargo nextest run --workspace --stress-count=5                # Run all tests, repeat several times to catch flaky tests
cs delta $(git merge-base HEAD origin/main)                   # Run CodeScene code quality analysis on current branch changes                                          
```

**Do not ignore clippy warnings.** CI sets `RUSTFLAGS=--deny warnings` and runs `cargo clippy -- -D warnings`, so any warning will fail the build.

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
feat: add harnx-mcp-todo as a file-based todo management MCP server
fix(acp): resolve ACP server hang and MCP transport death on Ctrl+C
chore(deps): update rust crate syntect to v5.3.0
```

## Changeset Files

When making a user-visible change, create a changeset file in `.changesets/`:

```markdown
---
harnx: minor
---
Brief description of the change.
```

The YAML front matter specifies the version bump: `patch`, `minor`, or `major`.

## Key Patterns

- **Error handling:** Use `anyhow::Result` / `anyhow::bail!` throughout.
- **Async:** All I/O is async via Tokio. Use `async fn` and `.await`.
- **Client modules:** Each LLM provider lives in `crates/harnx/src/client/` and follows the patterns in `client/common.rs` and `client/macros.rs`.
- **Configuration:** YAML-based; `config.yaml` holds global settings. Clients, MCP servers, and ACP servers each live in their own subdirectory (`clients/`, `mcp_servers/`, `acp_servers/`) as individual `<name>.yaml` files. Agents are Markdown files with YAML front matter in `agents/`. All agents are auto-registered as ACP servers.
- **Dual license:** MIT OR Apache-2.0. Preserve license headers where present.
