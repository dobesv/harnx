//! Local command events and generation-bound prompt output enter separate UI paths.

use std::sync::Arc;

use harnx_core::event::{AgentEvent, AgentEventSink};
use harnx_core::sink::install_agent_event_sink;

use crate::types::TuiEvent;

/// Prompt sinks retain the admitted generation and task identity across the
/// frontend queue. The startup sink is reserved for local commands/notices.
pub(crate) struct TuiAgentEventSink {
    tx: tokio::sync::mpsc::UnboundedSender<TuiEvent>,
    prompt: Option<(
        harnx_core::abort::AbortSignal,
        crate::event_isolation::EventStamp,
    )>,
}

impl TuiAgentEventSink {
    pub(crate) fn new(tx: tokio::sync::mpsc::UnboundedSender<TuiEvent>) -> Self {
        Self { tx, prompt: None }
    }
}

impl TuiAgentEventSink {
    pub(crate) fn for_prompt(
        tx: tokio::sync::mpsc::UnboundedSender<TuiEvent>,
        task: harnx_core::abort::AbortSignal,
        state: harnx_runtime::nats_event_sink::LiveEventState,
        execution_id: String,
    ) -> Self {
        Self {
            tx,
            prompt: Some((
                task,
                crate::event_isolation::EventStamp::live(&state, Some(execution_id)),
            )),
        }
    }
}

impl AgentEventSink for TuiAgentEventSink {
    fn emit(&self, event: AgentEvent) {
        match &self.prompt {
            Some((_, stamp)) => self.emit_live(
                event,
                stamp.execution_id.as_deref().expect("prompt identity"),
            ),
            None => {
                let _ = self.tx.send(TuiEvent::LocalAgent(event));
            }
        }
    }

    fn emit_live(&self, event: AgentEvent, execution_id: &str) {
        let Some((task, stamp)) = &self.prompt else {
            return;
        };
        if stamp.execution_id.as_deref() == Some(execution_id) && stamp.allows(&stamp.state) {
            let _ = self.tx.send(TuiEvent::Agent {
                task: task.clone(),
                stamp: stamp.clone(),
                event,
            });
        }
    }
}

/// Re-export of `harnx_runtime::utils::render_tool_result_text` so the
/// TUI sink and the CLI sink format tool results identically. The
/// returned text is NOT dim-wrapped — the TUI renderer applies the dim
/// `Modifier` via the `TranscriptItem::ToolResultMarkdown` render path.
pub(crate) fn render_tool_result_text(
    output: &serde_json::Value,
    markdown: Option<&str>,
) -> String {
    harnx_runtime::utils::render_tool_result_text(output, markdown)
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
        sink.emit(AgentEvent::sub_agent(
            AgentSource {
                model: None,
                agent: "argus".into(),
                session_id: Some("session-1".into()),
            },
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("hello".into())],
            }),
        ));
        let ev = rx.try_recv().expect("tui event");
        match ev {
            TuiEvent::LocalAgent(AgentEvent::SubAgent { source, event }) => {
                let AgentEvent::Model(ModelEvent::MessageChunk { blocks }) = *event else {
                    panic!("unexpected nested AgentEvent");
                };
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
        sink.emit(AgentEvent::Notice(NoticeEvent::Info("hi".into())));
        let ev = rx.try_recv().expect("tui event");
        match ev {
            TuiEvent::LocalAgent(AgentEvent::Notice(NoticeEvent::Info(msg))) => {
                assert_eq!(msg, "hi");
            }
            _ => panic!("unexpected TuiEvent"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn source_propagates_through_tool_completed() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TuiEvent>();
        let sink = TuiAgentEventSink::new(tx);
        sink.emit(AgentEvent::sub_agent(
            AgentSource {
                agent: "hephaestus".into(),
                session_id: None,
                model: None,
            },
            AgentEvent::Tool(ToolEvent::Completed {
                id: String::new(),
                output: serde_json::Value::String("ok".into()),
                markdown: None,
            }),
        ));
        let ev = rx.try_recv().expect("tui event");
        match ev {
            TuiEvent::LocalAgent(AgentEvent::SubAgent { source, event }) => {
                assert!(matches!(
                    *event,
                    AgentEvent::Tool(ToolEvent::Completed { .. })
                ));
                assert_eq!(source.agent, "hephaestus");
                assert!(source.session_id.is_none());
            }
            _ => panic!("unexpected TuiEvent"),
        }
    }
}
