//! Shared domain types, event model, and pure utilities used across the
//! harnx workspace. See the spec at
//! `docs/superpowers/specs/2026-04-19-monorepo-refactor-design.md` for the
//! role this crate plays in the multi-crate split.

pub mod abort;
pub mod agent_config;
pub mod agent_ref;
pub mod alloc_guard;
pub mod api_types;
pub mod attachments;
pub mod cli;
pub mod config_data;
pub mod config_paths;
pub mod context;
pub mod crypto;
pub mod error;
pub mod event;
pub mod hooks;
pub mod input;
pub mod instance;
pub mod last_message;
pub mod llm_trace;
pub mod macros;
pub mod message;
pub mod model;
pub mod package;
pub mod package_namespace;
pub mod path;
pub mod provider_config;
pub mod retry_config;
pub mod safety;
pub mod session;
pub mod session_log;
pub mod session_reconstruct;
pub mod sink;
pub mod system_vars;
pub mod text;
pub mod tool;
pub mod working_mode;

pub mod jaq;

/// Assert that the current test is running under `cargo nextest`, panicking with
/// guidance otherwise.
///
/// Some tests rely on per-test process isolation because they mutate
/// process-global state (environment variables, the shared model registry, tmux
/// sessions, etc.). `cargo test` runs every test of a binary in a single process
/// with shared threads, which makes these tests flaky and produces confusing
/// failures. `cargo nextest` runs each test in its own process and is the
/// supported runner.
///
/// Nextest sets `NEXTEST=1` in every test process; `cargo test` does not. Call
/// this as the first line of a test that must not run under `cargo test`.
#[track_caller]
pub fn require_nextest() {
    if std::env::var_os("NEXTEST").is_none() {
        panic!(
            "\n\n\
             This test must be run with cargo-nextest, not `cargo test`.\n\
             It mutates process-global state and needs nextest's per-test process isolation;\n\
             `cargo test` shares one process across tests and produces spurious failures.\n\n\
             Run instead:\n\
             \tcargo nextest run --workspace\n\n\
             See AGENTS.md (\"Verifying Changes\") for the full verification pipeline.\n"
        );
    }
}
