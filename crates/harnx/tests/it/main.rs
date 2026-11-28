//! All of this crate's integration tests, built as one binary. A separate
//! binary per file links the whole dependency graph again for each, which
//! dominated build time; nextest still runs every test in its own process.

mod cli_runner;
mod dump_attachment;
mod engine_smoke;
mod interact_cli;
mod interrupt_e2e;
mod openai_responses_e2e;
#[cfg(unix)]
mod patient_nats_cli;
mod session_cli_e2e;
mod session_dump_history_nats;
mod session_stats_cli_e2e;
mod tmux_e2e;
mod tool_cli;
mod validate_cli;
