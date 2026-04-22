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
use crate::ui_output::{UiOutputEvent, UiOutputEventKind};

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
        _ => crate::ui_output::pretty_yaml_block(output),
    });
    truncate_output(&output_str, &opts)
}

/// Install the `TuiAgentEventSink`. Called by TUI-mode startup with
/// the event channel sender so the sink can forward directly into the
/// TUI event loop. The legacy `install_ui_output_sender` bridge is
/// still installed alongside for tests that construct `UiOutputEvent`
/// directly (removed in Task 5 of Plan 38).
pub(crate) fn install_tui_agent_event_sink(
    tx: tokio::sync::mpsc::UnboundedSender<crate::tui::types::TuiEvent>,
) {
    install_agent_event_sink(Arc::new(TuiAgentEventSink::new(tx)));
    debug_assert!(
        has_agent_event_sink(),
        "TUI AgentEventSink must be installed after startup call"
    );
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
