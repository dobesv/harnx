//! harnx-side wrappers around `harnx_core::sink`. The process-level
//! `AGENT_EVENT_SINK` registry (install/emit/has/clear) lives in
//! harnx-core so harnx-engine can emit events directly without
//! reaching into harnx. This module keeps the harnx-only sink
//! implementations (CLI installer) and re-exports the core registry
//! API for existing call-site compatibility.
//!
//! The TUI-side sink (`TuiAgentEventSink` + `install_tui_agent_event_sink`)
//! has moved to `harnx-tui::agent_event_sink` (plan P49).
//!
//! The long-term design (see
//! `docs/superpowers/specs/2026-04-19-monorepo-refactor-design.md`)
//! threads `SessionCtx` through the engine for per-turn sink scoping.
//! This global exists as a bridge: legacy harnx emitters that haven't
//! migrated to `SessionCtx` plumbing can still reach the unified
//! event channel.

use std::sync::Arc;

pub use harnx_core::sink::{has_agent_event_sink, install_agent_event_sink};

use crate::cli_event_sink::CliAgentEventSink;
use crate::render::RenderOptions;

/// Install the stderr-backed `CliAgentEventSink`. Used by the CLI
/// (`Cmd`) working mode at process startup. Takes a `highlight` flag
/// and `RenderOptions` snapshot so the sink can render streaming
/// Model::MessageChunk / ThoughtChunk events directly to stdout (with
/// markdown rendering + raw-mode cursor manipulation on terminals).
pub fn install_cli_agent_event_sink(highlight: bool, render_options: RenderOptions) {
    install_agent_event_sink(Arc::new(CliAgentEventSink::new(highlight, render_options)));
    debug_assert!(
        has_agent_event_sink(),
        "CLI AgentEventSink must be installed after startup call"
    );
}
