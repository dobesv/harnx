//! harnx-runtime — Config-aware runtime glue extracted from the `harnx`
//! crate (plan P46, β+ progressive peel). Holds the `Config`, `Session`,
//! `Agent`, `Input` runtime types plus the provider-client orchestration
//! (`call_chat_completions`, retry/fallback), dot-commands dispatch,
//! and the `ToolEvalContext` bridge to `harnx-engine`.
//!
//! Downstream front-end crates (`harnx-serve`,
//! `harnx-tui`) depend on this crate rather than on `harnx` directly.

#[macro_use]
extern crate log;

pub mod agent_loop;
pub mod async_session_log;
pub mod bootstrap;
pub mod client;
pub mod commands;
pub mod config;
pub mod local_orchestrator;
pub mod nats_admin;
pub mod nats_client_session;
pub mod nats_event_sink;
pub mod nats_hook_provider;
pub mod nats_lease;
pub mod nats_local_server;
pub mod nats_metrics;
pub mod nats_session_index;
pub mod nats_session_log;
pub mod nats_tool_provider;
pub mod nats_worker;
pub mod remote_session_cleanup;
pub mod session_cleanup;
pub mod session_history;
pub mod test_utils;
pub mod tool;
mod tool_context;
pub mod utils;

// Re-export thin-client types for frontends
pub use nats_client_session::{
    send_control_command, ThinClientConfig, ThinClientSession, ThinClientTurnResult,
};
pub use nats_worker::ControlCommand;

pub use agent_loop::{
    continue_agent_loop_from_tool_round, run_agent_loop, run_agent_loop_with_local_handoff,
    AgentCallFn, AgentLoopContext, LoopResult, OnTextResponseFn, OnToolRoundFn,
    ToolApprovalDecision,
};

pub use tool::{ConfirmToolUseFn, ToolApprovalInterrupt, ToolUseConfirmation};
