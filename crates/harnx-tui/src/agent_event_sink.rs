//! TUI-side `AgentEventSink` implementation. Moved from `harnx::agent_event_sink`
//! (plan P49). The `TuiAgentEventSink` is a pure forwarder that pushes
//! `TuiEvent::Agent(event, source)` directly into the TUI event loop.

use std::sync::Arc;

use harnx_core::event::{AgentEvent, AgentEventSink, AgentSource};
use harnx_core::sink::install_agent_event_sink;

use crate::types::TuiEvent;

/// Sink used by the interactive TUI mode. Pure forwarder: carries an
/// `UnboundedSender<TuiEvent>` and pushes `TuiEvent::Agent(event, source)`
/// directly into the TUI event loop where `render_agent_event` dispatches
/// on the structured `AgentEvent` variants. No translation happens here.
pub(crate) struct TuiAgentEventSink {
    tx: tokio::sync::mpsc::UnboundedSender<TuiEvent>,
}

impl TuiAgentEventSink {
    pub(crate) fn new(tx: tokio::sync::mpsc::UnboundedSender<TuiEvent>) -> Self {
        Self { tx }
    }
}

impl AgentEventSink for TuiAgentEventSink {
    fn emit(&self, event: AgentEvent, source: Option<AgentSource>) {
        let _ = self.tx.send(TuiEvent::Agent(event, source));
    }
}

/// Re-export of `harnx_runtime::utils::render_tool_result_text` so the
/// TUI sink and the CLI sink format tool results identically. The
/// returned text is NOT dim-wrapped — the TUI renderer applies the dim
/// `Modifier` via `TranscriptItem::ToolResultText`'s style.
pub(crate) fn render_tool_result_text(output: &serde_json::Value, title: Option<&str>) -> String {
    harnx_runtime::utils::render_tool_result_text(output, title)
}

/// Install the `TuiAgentEventSink`. Called by TUI-mode startup with
/// the event channel sender so the sink can forward directly into the
/// TUI event loop.
pub(crate) fn install_tui_agent_event_sink(tx: tokio::sync::mpsc::UnboundedSender<TuiEvent>) {
    install_agent_event_sink(Arc::new(TuiAgentEventSink::new(tx)));
    debug_assert!(
        harnx_core::sink::has_agent_event_sink(),
        "TUI AgentEventSink must be installed after startup call"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TuiEvent;
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
                title: None,
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
