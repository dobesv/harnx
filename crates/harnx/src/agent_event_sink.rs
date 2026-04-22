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
use crate::render::RenderOptions;
use crate::ui_output::{emit_ui_output_event, UiOutputEvent, UiOutputEventKind};

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

/// Sink used by the interactive TUI mode. Translates `AgentEvent`
/// into the legacy `UiOutputEvent` representation and forwards to
/// the existing TUI consumer via `emit_ui_output_event`.
///
/// This bridge exists because the TUI's event-loop still consumes
/// `UiOutputEvent` today. When the TUI migrates to consume `AgentEvent`
/// directly, this struct can be replaced with a thinner renderer.
pub struct TuiAgentEventSink;

impl AgentEventSink for TuiAgentEventSink {
    fn emit(&self, event: AgentEvent, source: Option<harnx_core::event::AgentSource>) {
        use harnx_core::event::{ModelEvent, NoticeEvent, ToolEvent};
        let ui_source = source.map(|s| crate::ui_output::UiOutputSource {
            agent: s.agent,
            session_id: s.session_id,
        });
        match event {
            AgentEvent::Notice(notice) => {
                let text = match notice {
                    NoticeEvent::Info(msg) => msg,
                    NoticeEvent::Warning(msg) => format!("⚠ {msg}"),
                    NoticeEvent::Error(msg) => format!("error: {msg}"),
                };
                let ui_event = UiOutputEvent {
                    kind: UiOutputEventKind::TranscriptText { text },
                    source: ui_source,
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
                    source: ui_source,
                };
                emit_ui_output_event(ui_event);
            }
            AgentEvent::Tool(ToolEvent::Completed {
                output, content, ..
            }) => {
                let text = render_tool_result_text(&output, &content);
                let ui_event = UiOutputEvent {
                    kind: UiOutputEventKind::ToolResultText { text },
                    source: ui_source,
                };
                emit_ui_output_event(ui_event);
            }
            AgentEvent::Model(ModelEvent::MessageChunk { blocks }) => {
                let text = concat_text_blocks(&blocks);
                if !text.is_empty() {
                    let ui_event = UiOutputEvent {
                        kind: UiOutputEventKind::MessageChunk { text, raw: None },
                        source: ui_source,
                    };
                    emit_ui_output_event(ui_event);
                }
            }
            AgentEvent::Model(ModelEvent::ThoughtChunk { blocks }) => {
                let text = concat_text_blocks(&blocks);
                if !text.is_empty() {
                    let ui_event = UiOutputEvent {
                        kind: UiOutputEventKind::ThoughtChunk { text, raw: None },
                        source: ui_source,
                    };
                    emit_ui_output_event(ui_event);
                }
            }
            // Model (other variants), Turn, Session, Status, Plan, other
            // Tool variants (Progress/Update/Failed): dropped by this
            // bridge. The TUI consumer still receives everything it needs
            // through UiOutputEvent emitted directly by harnx code.
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

/// Concatenate `ContentBlock::Text(..)` fragments into a single String.
/// Non-Text blocks (Image, ResourceLink, Opaque) are skipped — the TUI
/// transcript currently only renders text.
fn concat_text_blocks(blocks: &[harnx_core::event::ContentBlock]) -> String {
    use harnx_core::event::ContentBlock;
    let mut out = String::new();
    for block in blocks {
        if let ContentBlock::Text(t) = block {
            out.push_str(t);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::event::{AgentSource, ContentBlock, ModelEvent, NoticeEvent, ToolEvent};

    fn install_collector() -> tokio::sync::mpsc::UnboundedReceiver<UiOutputEvent> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        crate::ui_output::install_ui_output_sender(tx);
        rx
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn source_propagates_through_message_chunk() {
        let mut rx = install_collector();
        let sink = TuiAgentEventSink;
        sink.emit(
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("hello".into())],
            }),
            Some(AgentSource {
                agent: "argus".into(),
                session_id: Some("session-1".into()),
            }),
        );
        let ev = rx.try_recv().expect("ui output event");
        match &ev.kind {
            UiOutputEventKind::MessageChunk { text, .. } => assert_eq!(text, "hello"),
            other => panic!("unexpected kind: {other:?}"),
        }
        let source = ev.source.expect("source preserved");
        assert_eq!(source.agent, "argus");
        assert_eq!(source.session_id.as_deref(), Some("session-1"));
        crate::ui_output::clear_ui_output_sender();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn none_source_yields_none_source() {
        let mut rx = install_collector();
        let sink = TuiAgentEventSink;
        sink.emit(AgentEvent::Notice(NoticeEvent::Info("hi".into())), None);
        let ev = rx.try_recv().expect("ui output event");
        assert!(ev.source.is_none());
        crate::ui_output::clear_ui_output_sender();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn source_propagates_through_tool_completed() {
        let mut rx = install_collector();
        let sink = TuiAgentEventSink;
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
        let ev = rx.try_recv().expect("ui output event");
        match &ev.kind {
            UiOutputEventKind::ToolResultText { .. } => {}
            other => panic!("unexpected kind: {other:?}"),
        }
        let source = ev.source.expect("source preserved");
        assert_eq!(source.agent, "hephaestus");
        assert!(source.session_id.is_none());
        crate::ui_output::clear_ui_output_sender();
    }
}
