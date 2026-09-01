//! harnx-session — Shared session management machinery for harnx frontends.
//!
//! Provides unified per-session state management that both servers consume:
//! - Config forking: `fork_prompt_config` unifies the fork pattern from both servers
//! - AgentLoopContext building: `build_context` constructs context for agent loop runs
//!
//! Transport-specific concerns (broadcast/SSE notification, session registry/reap)
//! remain in the respective server crates behind adapter traits.

use harnx_core::abort::AbortSignal;
use harnx_runtime::{config::GlobalConfig, AgentLoopContext, OnToolRoundFn};
use std::path::PathBuf;

pub mod config;

pub use config::fork_prompt_config;

/// Build an AgentLoopContext suitable for running the agent loop.
///
/// This function constructs the context with:
/// - Optional call_fn override (None → use default client call)
/// - Optional on_tool_round callback for injecting text mid-turn
/// - Optional working_dir for per-session CWD isolation
pub fn build_context(
    prompt_config: GlobalConfig,
    call_fn: Option<harnx_runtime::AgentCallFn>,
    abort_signal: AbortSignal,
    on_tool_round: Option<OnToolRoundFn>,
    working_dir: Option<PathBuf>,
) -> AgentLoopContext {
    AgentLoopContext {
        instance_id: harnx_core::instance::ServerScope::new(),
        config: prompt_config,
        abort_signal,
        token_budget: None,
        usage_at_start: Default::default(),
        call_fn,
        on_tool_round,
        on_text_response: None,
        initial_with_embeddings: true,
        initial_resume_count: 0,
        max_resume: None,
        nats_hook_provider: None,
        pending_async_context: None,
        working_dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_runtime::config::Config;

    #[test]
    fn fork_prompt_config_creates_isolated_config() {
        let base = Config::default();
        let forked = fork_prompt_config(&base);
        // Forked config should be a fresh Arc with session scope cleared
        assert!(forked.read().session.is_none());
    }
}
