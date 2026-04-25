//! harnx-runtime — Config-aware runtime glue extracted from the `harnx`
//! crate (plan P46, β+ progressive peel). Holds the `Config`, `Session`,
//! `Agent`, `Input` runtime types plus the provider-client orchestration
//! (`call_chat_completions`, retry/fallback), dot-commands dispatch,
//! and the `ToolEvalContext` bridge to `harnx-engine`.
//!
//! Downstream front-end crates (`harnx-serve`, `harnx-acp-server`,
//! `harnx-tui`) depend on this crate rather than on `harnx` directly.

#[macro_use]
extern crate log;

pub mod agent_loop;
pub mod bootstrap;
pub mod client;
pub mod commands;
pub mod config;
pub mod test_utils;
pub mod tool;
pub mod utils;

pub use agent_loop::{
    run_agent_loop, AgentCallFn, AgentLoopContext, OnTextResponseFn, OnToolRoundFn,
};
