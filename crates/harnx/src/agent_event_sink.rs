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

/// Sink used by the interactive TUI mode. Pure forwarder: carries an
/// `UnboundedSender<TuiEvent>` and pushes `TuiEvent::Agent(event, source)`
/// directly into the TUI event loop where `render_agent_event` dispatches
/// on the structured `AgentEvent` variants. No translation happens here.
pub(crate) struct TuiAgentEventSink {
    tx: tokio::sync::mpsc::UnboundedSender<crate::tui::types::TuiEvent>,
}

impl TuiAgentEventSink {
    pub(crate) fn new(tx: tokio::sync::mpsc::UnboundedSender<crate::tui::types::TuiEvent>) -> Self {
        Self { tx }
    }
}

impl AgentEventSink for TuiAgentEventSink {
    fn emit(&self, event: AgentEvent, source: Option<harnx_core::event::AgentSource>) {
        let _ = self
            .tx
            .send(crate::tui::types::TuiEvent::Agent(event, source));
    }
}

/// Extract and truncate a tool result for transcript display. Used by
/// `render_agent_event`'s `Tool::Completed` arm in `tui/input.rs`. The
/// returned text is NOT dim-wrapped — the TUI renderer applies the dim
/// `Modifier` via `TranscriptItem::ToolResultText`'s style so an ANSI
/// dim escape would be redundant (and would fight test inputs that
/// pre-dim their text — `sanitize_output_text` strips the ESC, leaving
/// literal `[2m`/`[0m` markers visible).
///
/// Mirrors the head/line sizing used by `default_emit_tool_result` so
/// production and TUI transcripts truncate at the same boundary.
pub(crate) fn render_tool_result_text(
    output: &serde_json::Value,
    _content: &[harnx_core::event::ContentBlock],
) -> String {
    use crate::mcp_safety::{truncate_output, TruncateOpts};
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
        _ => crate::utils::pretty_yaml_block(output),
    });
    truncate_output(&output_str, &opts)
}

/// Install the `TuiAgentEventSink`. Called by TUI-mode startup with
/// the event channel sender so the sink can forward directly into the
/// TUI event loop.
pub(crate) fn install_tui_agent_event_sink(
    tx: tokio::sync::mpsc::UnboundedSender<crate::tui::types::TuiEvent>,
) {
    install_agent_event_sink(Arc::new(TuiAgentEventSink::new(tx)));
    debug_assert!(
        has_agent_event_sink(),
        "TUI AgentEventSink must be installed after startup call"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::types::TuiEvent;
    use harnx_core::event::{AgentSource, ContentBlock, ModelEvent, NoticeEvent, ToolEvent};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn source_propagates_through_message_chunk() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TuiEvent>();
        let sink = TuiAgentEventSink::new(tx);
        sink.emit(
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("hello".into())],
            }),
            Some(AgentSource {
                agent: "argus".into(),
                session_id: Some("session-1".into()),
            }),
        );
        let ev = rx.try_recv().expect("tui event");
        match ev {
            TuiEvent::Agent(
                AgentEvent::Model(ModelEvent::MessageChunk { blocks }),
                Some(source),
            ) => {
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    ContentBlock::Text(t) => assert_eq!(t, "hello"),
                    other => panic!("unexpected block: {other:?}"),
                }
                assert_eq!(source.agent, "argus");
                assert_eq!(source.session_id.as_deref(), Some("session-1"));
            }
            _ => panic!("unexpected TuiEvent"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn none_source_yields_none_source() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TuiEvent>();
        let sink = TuiAgentEventSink::new(tx);
        sink.emit(AgentEvent::Notice(NoticeEvent::Info("hi".into())), None);
        let ev = rx.try_recv().expect("tui event");
        match ev {
            TuiEvent::Agent(AgentEvent::Notice(NoticeEvent::Info(msg)), None) => {
                assert_eq!(msg, "hi");
            }
            _ => panic!("unexpected TuiEvent"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn source_propagates_through_tool_completed() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TuiEvent>();
        let sink = TuiAgentEventSink::new(tx);
        sink.emit(
            AgentEvent::Tool(ToolEvent::Completed {
                id: String::new(),
                output: serde_json::Value::String("ok".into()),
                content: vec![],
            }),
            Some(AgentSource {
                agent: "hephaestus".into(),
                session_id: None,
            }),
        );
        let ev = rx.try_recv().expect("tui event");
        match ev {
            TuiEvent::Agent(AgentEvent::Tool(ToolEvent::Completed { .. }), Some(source)) => {
                assert_eq!(source.agent, "hephaestus");
                assert!(source.session_id.is_none());
            }
            _ => panic!("unexpected TuiEvent"),
        }
    }
}
