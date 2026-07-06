//! harnx-session — Shared session management machinery for harnx-serve and harnx-acp-server.
//!
//! Provides unified per-session state management that both servers consume:
//! - Config forking: `fork_prompt_config` unifies the fork pattern from both servers
//! - AgentLoopContext building: `build_context` constructs context for agent loop runs
//!
//! Transport-specific concerns (broadcast/SSE/ACP notification, session registry/reap)
//! remain in the respective server crates behind adapter traits.

use harnx_core::abort::AbortSignal;
use harnx_hooks::{AsyncHookManager, PersistentHookManager};
use harnx_runtime::{config::GlobalConfig, AgentLoopContext, OnToolRoundFn};
use std::{path::PathBuf, sync::Arc};

pub mod config;

pub use config::fork_prompt_config;

/// Build an AgentLoopContext suitable for running the agent loop.
///
/// This function constructs the context with:
/// - Default async/persistent hook managers (fresh per-run)
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
        config: prompt_config,
        abort_signal,
        async_manager: Arc::new(tokio::sync::Mutex::new(AsyncHookManager::default())),
        persistent_manager: Arc::new(tokio::sync::Mutex::new(PersistentHookManager::default())),
        call_fn,
        on_tool_round,
        on_text_response: None,
        initial_with_embeddings: true,
        initial_resume_count: 0,
        max_resume: None,
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
