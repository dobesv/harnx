//! Config forking utilities for session-scoped prompt configs.
//!
//! Provides a unified config fork function used by harnx frontends
//! to create isolated per-prompt GlobalConfig instances from a shared base Config.

use harnx_runtime::config::{Config, GlobalConfig};
use parking_lot::RwLock;
use std::sync::Arc;

/// Fork a base Config into a fresh GlobalConfig suitable for per-session use.
///
/// This creates an isolated Arc<RwLock<Config>> from the base config using
/// `fork_session_scope()`, which:
/// - Clones all shared resources (clients, managers, etc.)
/// - Clears session state (each fork starts with session: None)
/// - Drops TUI-only hooks that can't be cloned
///
/// The caller then calls `use_agent_by_name` and `use_session` to bind the
/// forked config to a specific agent/session before running a prompt.
pub fn fork_prompt_config(base: &Config) -> GlobalConfig {
    Arc::new(RwLock::new(base.fork_session_scope()))
}
