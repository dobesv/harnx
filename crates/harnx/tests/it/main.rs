//! All of this crate's integration tests, built as one binary. A separate
//! binary per file links the whole dependency graph again for each, which
//! dominated build time; nextest still runs every test in its own process.

mod dump_attachment;
mod engine_smoke;
mod interrupt_e2e;
mod openai_responses_e2e;
mod session_cli_e2e;
mod tmux_e2e;
