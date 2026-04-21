//! harnx-side wrappers around `harnx_core::sink`. The process-level
//! `AGENT_EVENT_SINK` registry (install/emit/has/clear) lives in
//! harnx-core so harnx-engine can emit events directly without
//! reaching into harnx. This module keeps the harnx-only sink
//! implementations (`TuiAgentEventSink` + CLI/TUI installers) and
//! re-exports the core registry API for existing call-site
//! compatibility.
//!
//! The long-term design (see
//! `docs/superpowers/specs/2026-04-19-monorepo-refactor-design.md`)
//! threads `SessionCtx` through the engine for per-turn sink scoping.
//! This global exists as a bridge: legacy harnx emitters that haven't
//! migrated to `SessionCtx` plumbing can still reach the unified
//! event channel.

use std::sync::Arc;

use harnx_core::event::AgentEvent;
use harnx_core::event::AgentEventSink;

pub use harnx_core::sink::{emit_agent_event, has_agent_event_sink, install_agent_event_sink};

use crate::cli_event_sink::CliAgentEventSink;
use crate::ui_output::{emit_ui_output_event, UiOutputEvent, UiOutputEventKind};

/// Install the stderr-backed `CliAgentEventSink`. Used by the CLI
/// (`Cmd`) working mode at process startup.
pub fn install_cli_agent_event_sink() {
    install_agent_event_sink(Arc::new(CliAgentEventSink));
    debug_assert!(
        has_agent_event_sink(),
        "CLI AgentEventSink must be installed after startup call"
    );
}

/// Sink used by the interactive TUI mode. Translates `AgentEvent`
/// into the legacy `UiOutputEvent` representation and forwards to
/// the existing TUI consumer via `emit_ui_output_event`.
///
/// This bridge exists because the TUI's event-loop still consumes
/// `UiOutputEvent` today. When the TUI migrates to consume `AgentEvent`
/// directly, this struct can be replaced with a thinner renderer.
pub struct TuiAgentEventSink;

impl AgentEventSink for TuiAgentEventSink {
    fn emit(&self, event: AgentEvent) {
        use harnx_core::event::NoticeEvent;
        // Only Notice variants need translation today — retry warnings
        // and similar operator-facing messages. Other variants (Turn,
        // Model, Tool, Session, Status, Plan) are still emitted via
        // `emit_ui_output_event` directly by the harnx code that
        // creates them. When those call sites migrate to AgentEvent,
        // add translation branches here.
        if let AgentEvent::Notice(notice) = event {
            let text = match notice {
                NoticeEvent::Info(msg) => msg,
                NoticeEvent::Warning(msg) => format!("⚠ {msg}"),
                NoticeEvent::Error(msg) => format!("error: {msg}"),
            };
            let ui_event = UiOutputEvent {
                kind: UiOutputEventKind::TranscriptText { text },
                source: None,
            };
            emit_ui_output_event(ui_event);
        }
    }
}

/// Install the `TuiAgentEventSink`. Called by TUI-mode startup
/// alongside the existing `install_ui_output_sender` for the legacy
/// `UiOutputEvent` channel.
pub fn install_tui_agent_event_sink() {
    install_agent_event_sink(Arc::new(TuiAgentEventSink));
    debug_assert!(
        has_agent_event_sink(),
        "TUI AgentEventSink must be installed after startup call"
    );
}
