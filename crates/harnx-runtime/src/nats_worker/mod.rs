//! NATS worker entrypoint.
//!
//! P1.3 implementation: drives `run_agent_loop` with NATS-backed session persistence.
//!
//! ## Persistence seam design
//!
//! The existing `run_agent_loop` persists session entries through:
//!   `Config::after_chat_completion` → `save_message` → `session::add_assistant_text`
//!   → `append_event(session, entry)` → active session append sink
//!
//! For NATS mode, we need to redirect this sync file write to async JetStream.
//! Since `run_agent_loop` is async but the persistence path is sync, we use
//! `tokio::task::block_in_place` to safely block on async NATS calls.
//!
//! **Seam:** Add `session_log_backend: SessionLogBackend` field to `AgentLoopContext`.
//! The loop's persistence calls remain unchanged. When the backend is `Nats`,
//! `Config::save_message` uses `block_in_place` to await the async NATS append.
//!
//! This approach:
//! - Keeps LOCAL mode byte-identical (`SessionLogBackend::Local` → sync file write)
//! - Adds minimal changes to the loop (just the backend enum check in Config)
//! - Supports future HA/lease wraps around the same `session_log_backend` seam

mod agent_loop;
mod backend;
mod control;
mod daemon;
mod diagnostics;
mod hook_supervisor;
mod subagent_toolset;
mod tool_registry;
mod tool_supervisor;

#[cfg(test)]
mod session_start_hook_tests;
#[cfg(test)]
mod tests;

// Re-export public items to preserve the `crate::nats_worker::X` path
pub use agent_loop::{
    reconcile_hook_supervisor, run_agent_loop_with_nats, run_agent_loop_with_nats_inner,
    RunAgentLoopArgs,
};
pub use backend::{FencedSessionLogSink, NatsSessionLogBackend};
pub use control::{control_subject, publish_control_command, ControlCommand};
pub use daemon::{
    new_remote_session_id, notify_subject, publish_session_activate, run_worker_daemon,
    worker_ready_subject, SessionActivate, WorkerDaemonConfig,
};
pub use diagnostics::diagnose_tool_servers;
#[doc(hidden)]
pub use hook_supervisor::publish_crash_rejector;
pub use hook_supervisor::{HookServerStartConfig, HookServerSupervisor};
pub use tool_supervisor::{ToolServerStartConfig, ToolServerSupervisor};
