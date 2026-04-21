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
        use harnx_core::event::{NoticeEvent, ToolEvent};
        match event {
            AgentEvent::Notice(notice) => {
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
            AgentEvent::Tool(ToolEvent::Started { name, input, .. }) => {
                let ui_event = UiOutputEvent {
                    kind: UiOutputEventKind::ToolCall {
                        tool_name: name,
                        input_yaml: match &input {
                            serde_json::Value::Null => None,
                            _ => Some(crate::ui_output::pretty_yaml_block(&input)),
                        },
                        raw: None,
                    },
                    source: None,
                };
                emit_ui_output_event(ui_event);
            }
            AgentEvent::Tool(ToolEvent::Completed {
                output, content, ..
            }) => {
                let text = render_tool_result_text(&output, &content);
                let ui_event = UiOutputEvent {
                    kind: UiOutputEventKind::ToolResultText { text },
                    source: None,
                };
                emit_ui_output_event(ui_event);
            }
            // Model, Turn, Session, Status, Plan, other Tool variants
            // (Progress/Update/Failed): dropped by this bridge. The TUI
            // consumer still receives everything it needs through
            // UiOutputEvent emitted directly by harnx code.
            _ => {}
        }
    }
}

/// Shared Completed formatter. Reconstructs the truncated + dimmed text
/// that harnx's legacy `default_emit_tool_result` emitted. Used by the
/// TUI bridge today; CliAgentEventSink may adopt this later if CLI
/// tool-result rendering grows beyond the compact one-liner.
fn render_tool_result_text(
    output: &serde_json::Value,
    _content: &[harnx_core::event::ContentBlock],
) -> String {
    use crate::mcp_safety::{truncate_output, TruncateOpts};
    use crate::utils::dimmed_text;
    use harnx_core::tool::extract_user_display_text;

    let mut opts = TruncateOpts::default();
    let marker = " [...] ";
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        opts.head_lines = 5.max((rows / 2) as usize);
        opts.tail_lines = 0;
        opts.line_head_bytes = (cols as usize).saturating_sub(3 + marker.len());
        opts.line_tail_bytes = 0;
        opts.marker = Some(marker.to_string());
    }
    let output_str = extract_user_display_text(output).unwrap_or_else(|| match output {
        serde_json::Value::String(s) => s.clone(),
        _ => crate::ui_output::pretty_yaml_block(output),
    });
    let truncated = truncate_output(&output_str, &opts);
    format!("{}\n", dimmed_text(&truncated))
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
