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

/// Translate a legacy `UiOutputEvent` into its `AgentEvent` counterpart.
/// Used by `forward_acp_chunks` to route ACP sub-agent output through
/// the unified AgentEvent sink.
///
/// **Known limitations** (preserved from the pre-migration
/// `UiOutputEvent` contract; not new regressions):
/// - `Tool::Started` and `Tool::Completed` carry `id: String::new()`.
///   `UiOutputEventKind::ToolCall` and `ToolResultText` have no tool
///   call id field upstream, so no correlation id is available. Current
///   sinks (Cli/Tui) do not correlate Started↔Completed by id, so this
///   is safe. If a future consumer needs id correlation for ACP-sourced
///   tool events, extract the id from `UiOutputEventKind::ToolCall.raw:
///   Option<Box<acp::ToolCall>>`. `ToolCallUpdate` already preserves
///   its `tool_call_id` (see arm below).
/// - `yaml_to_json_value` falls back to `Value::String(raw_yaml)` on
///   parse failure. ACP-originated YAML is emitted via
///   `pretty_yaml_block` and is always valid, so the fallback is a
///   safety net rather than an expected path. If downstream consumers
///   need structured indexing, malformed YAML surfaces as a type
///   mismatch at the consumer — not silently corrected data.
/// - `parse_tool_status` accepts both snake_case, PascalCase, and the
///   `in-progress` kebab form. ACP protocol uses snake_case, but the
///   `UiOutputEventKind::ToolCallUpdate.status: Option<String>` is
///   unvalidated at construction, so the parser is permissive.
pub fn ui_output_to_agent_event(event: UiOutputEvent) -> AgentEvent {
    use harnx_core::event::{
        ContentBlock, ModelEvent, NoticeEvent, PlanEntry, ToolEvent, ToolKind,
    };
    match event.kind {
        UiOutputEventKind::MessageChunk { text, .. } => {
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text(text)],
            })
        }
        UiOutputEventKind::ThoughtChunk { text, .. } => {
            AgentEvent::Model(ModelEvent::ThoughtChunk {
                blocks: vec![ContentBlock::Text(text)],
            })
        }
        UiOutputEventKind::TranscriptText { text } => AgentEvent::Notice(NoticeEvent::Info(text)),
        UiOutputEventKind::ToolResultText { text } => AgentEvent::Tool(ToolEvent::Completed {
            id: String::new(),
            output: serde_json::Value::String(text),
            content: vec![],
        }),
        UiOutputEventKind::ToolCall {
            tool_name,
            input_yaml,
            ..
        } => AgentEvent::Tool(ToolEvent::Started {
            id: String::new(),
            name: tool_name,
            kind: ToolKind::Other,
            title: None,
            input: yaml_to_json_value(input_yaml.as_deref()),
            locations: vec![],
        }),
        UiOutputEventKind::ToolCallUpdate {
            tool_call_id,
            title,
            status,
            ..
        } => AgentEvent::Tool(ToolEvent::Update {
            id: tool_call_id.unwrap_or_default(),
            title,
            status: status.as_deref().and_then(parse_tool_status),
            content: None,
        }),
        UiOutputEventKind::LlmFinal { output, usage } => {
            AgentEvent::Model(ModelEvent::Final { output, usage })
        }
        UiOutputEventKind::LlmError(text) => AgentEvent::Model(ModelEvent::Error(text)),
        UiOutputEventKind::Plan { entries } => AgentEvent::Plan {
            entries: entries
                .into_iter()
                .map(|e| PlanEntry {
                    status: e.status,
                    content: e.content,
                })
                .collect(),
        },
        UiOutputEventKind::Usage {
            input_tokens,
            output_tokens,
            cached_tokens,
            session_label,
        } => AgentEvent::Model(ModelEvent::Usage {
            input: input_tokens,
            output: output_tokens,
            cached: cached_tokens,
            session_label,
        }),
    }
}

fn yaml_to_json_value(yaml: Option<&str>) -> serde_json::Value {
    match yaml {
        None | Some("") => serde_json::Value::Null,
        Some(s) => serde_yaml::from_str::<serde_json::Value>(s)
            .unwrap_or(serde_json::Value::String(s.to_string())),
    }
}

fn parse_tool_status(status: &str) -> Option<harnx_core::event::ToolStatus> {
    use harnx_core::event::ToolStatus;
    match status {
        "pending" | "Pending" => Some(ToolStatus::Pending),
        "in_progress" | "InProgress" | "in-progress" => Some(ToolStatus::InProgress),
        "completed" | "Completed" => Some(ToolStatus::Completed),
        "failed" | "Failed" => Some(ToolStatus::Failed),
        _ => None,
    }
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

    #[test]
    fn translate_message_chunk_to_agent_event() {
        use harnx_core::event::{ContentBlock, ModelEvent};
        let ui = UiOutputEvent {
            kind: UiOutputEventKind::MessageChunk {
                text: "hello".into(),
                raw: None,
            },
            source: None,
        };
        match ui_output_to_agent_event(ui) {
            AgentEvent::Model(ModelEvent::MessageChunk { blocks }) => {
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    ContentBlock::Text(t) => assert_eq!(t, "hello"),
                    other => panic!("unexpected block: {other:?}"),
                }
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn translate_transcript_text_to_notice_info() {
        let ui = UiOutputEvent {
            kind: UiOutputEventKind::TranscriptText {
                text: "note".into(),
            },
            source: None,
        };
        match ui_output_to_agent_event(ui) {
            AgentEvent::Notice(NoticeEvent::Info(msg)) => assert_eq!(msg, "note"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn translate_tool_call_yaml_to_json() {
        use harnx_core::event::{ToolEvent, ToolKind};
        let ui = UiOutputEvent {
            kind: UiOutputEventKind::ToolCall {
                tool_name: "read_file".into(),
                input_yaml: Some("path: /tmp/x.txt\nfollow: true".into()),
                raw: None,
            },
            source: None,
        };
        match ui_output_to_agent_event(ui) {
            AgentEvent::Tool(ToolEvent::Started {
                name, kind, input, ..
            }) => {
                assert_eq!(name, "read_file");
                assert!(matches!(kind, ToolKind::Other));
                assert_eq!(input["path"], "/tmp/x.txt");
                assert_eq!(input["follow"], true);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn translate_tool_call_update_maps_status() {
        use harnx_core::event::{ToolEvent, ToolStatus};
        let ui = UiOutputEvent {
            kind: UiOutputEventKind::ToolCallUpdate {
                tool_call_id: Some("call-42".into()),
                title: Some("reading file".into()),
                status: Some("in_progress".into()),
                raw: None,
            },
            source: None,
        };
        match ui_output_to_agent_event(ui) {
            AgentEvent::Tool(ToolEvent::Update {
                id, title, status, ..
            }) => {
                assert_eq!(id, "call-42");
                assert_eq!(title.as_deref(), Some("reading file"));
                assert!(matches!(status, Some(ToolStatus::InProgress)));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn translate_unknown_tool_status_yields_none() {
        use harnx_core::event::ToolEvent;
        let ui = UiOutputEvent {
            kind: UiOutputEventKind::ToolCallUpdate {
                tool_call_id: None,
                title: None,
                status: Some("mystery".into()),
                raw: None,
            },
            source: None,
        };
        match ui_output_to_agent_event(ui) {
            AgentEvent::Tool(ToolEvent::Update { status, .. }) => {
                assert!(status.is_none());
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn translate_plan_entries() {
        let ui = UiOutputEvent {
            kind: UiOutputEventKind::Plan {
                entries: vec![crate::ui_output::UiOutputPlanEntry {
                    status: "done".into(),
                    content: "first step".into(),
                }],
            },
            source: None,
        };
        match ui_output_to_agent_event(ui) {
            AgentEvent::Plan { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].status, "done");
                assert_eq!(entries[0].content, "first step");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
