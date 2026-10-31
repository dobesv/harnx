//! ACP event sink implementing AgentEventSink for in-order streaming.
//!
//! This implements the "single sequential drain loop" pattern from PR #1038:
//! NO per-chunk tokio::spawn. Instead, we use an unbounded channel to collect
//! events and a single drain task that sends session/update notifications in order.
//!
//! Key invariant: Drop the sink sender and await the drain task before
//! finishing the prompt turn to ensure all chunks flush in order.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::SessionUpdate;
use harnx_core::event::{AgentEvent, AgentEventSink};
use tokio::sync::mpsc;

use crate::event_map::{agent_event_to_replay_update, agent_event_to_session_update_for_cluster};
use crate::handoff::{committed_target, fallback_update};
use crate::SessionContext;

/// Internal message sent from the sink to the drain task.
#[derive(Debug)]
pub enum AcpMessage {
    /// Send a session/update notification with content.
    Update {
        session_id: String,
        update: Box<SessionUpdate>,
    },
    /// Turn completed signal (drain should flush and stop).
    TurnComplete,
}

/// A handle to signal turn completion without owning the sink.
///
/// The sink is typically wrapped in `Arc<dyn AgentEventSink>` and passed
/// to `follow_admitted_prompt`. This handle allows the caller to signal
/// completion after the turn finishes, before awaiting the drain task.
#[derive(Clone)]
pub struct SignalHandle {
    tx: mpsc::UnboundedSender<AcpMessage>,
}

impl SignalHandle {
    /// Signal that the turn has completed.
    ///
    /// This sends a `TurnComplete` message to the drain task, which will
    /// flush any remaining messages and exit cleanly.
    pub fn signal_complete(&self) {
        let _ = self.tx.send(AcpMessage::TurnComplete);
    }
}

/// An event sink that forwards AgentEvents to ACP session/update notifications.
///
/// Uses a single sequential drain loop via unbounded channel to ensure
/// events are sent in order (PR #1038 fix).
pub struct AcpEventSink {
    tx: mpsc::UnboundedSender<AcpMessage>,
    session_id: String,
    source_cluster: String,
    session: Option<Arc<SessionContext>>,
    replay: bool,
    terminal_tool_call_ids: Mutex<HashSet<String>>,
}

impl AcpEventSink {
    /// Create a new event sink for an ACP session.
    ///
    /// The sender should drop the sink and await the drain task to flush
    /// all pending updates before completing the prompt turn.
    pub fn new(session_id: String) -> (Self, mpsc::UnboundedReceiver<AcpMessage>) {
        Self::build(
            session_id,
            harnx_runtime::config::LOCAL_CLUSTER_KEY.to_string(),
            None,
            false,
        )
    }

    /// Create a sink that deactivates source state on an authoritative commit.
    pub fn for_session(
        session_id: String,
        source_cluster: String,
        session: Arc<SessionContext>,
    ) -> (Self, mpsc::UnboundedReceiver<AcpMessage>) {
        Self::build(session_id, source_cluster, Some(session), false)
    }

    /// Create a read-only transcript replay sink.
    pub fn for_replay(
        session_id: String,
        source_cluster: String,
    ) -> (Self, mpsc::UnboundedReceiver<AcpMessage>) {
        Self::build(session_id, source_cluster, None, true)
    }

    fn build(
        session_id: String,
        source_cluster: String,
        session: Option<Arc<SessionContext>>,
        replay: bool,
    ) -> (Self, mpsc::UnboundedReceiver<AcpMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                session_id,
                source_cluster,
                session,
                replay,
                terminal_tool_call_ids: Mutex::new(HashSet::new()),
            },
            rx,
        )
    }

    /// Get the session ID this sink is associated with.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Create a signal handle that can signal turn completion.
    ///
    /// The handle can be used after the sink is moved into an Arc.
    /// Call `signal_handle.signal_complete()` after the turn completes
    /// and before awaiting the drain task.
    pub fn signal_handle(&self) -> SignalHandle {
        SignalHandle {
            tx: self.tx.clone(),
        }
    }

    /// Tell the sequential drain that no more turn events will be emitted.
    pub fn signal_complete(&self) {
        let _ = self.tx.send(AcpMessage::TurnComplete);
    }

    fn terminal_ids(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.terminal_tool_call_ids
            .lock()
            .expect("terminal tool call lock poisoned")
    }

    fn record_terminal(&self, event: &AgentEvent) {
        use harnx_core::event::ToolEvent;
        let AgentEvent::Tool(ToolEvent::Completed { id, .. } | ToolEvent::Failed { id, .. }) =
            innermost_event(event)
        else {
            return;
        };
        self.terminal_ids().insert(id.clone());
    }

    fn strip_late_progress_status(&self, id: Option<&str>, update: &mut SessionUpdate) {
        let Some(id) = id else {
            return;
        };
        let is_terminal = self.terminal_ids().contains(id);
        if !is_terminal {
            return;
        }
        let SessionUpdate::ToolCallUpdate(update) = update else {
            return;
        };
        // Late reconcile progress (such as recovered child Done) must not reopen a finished call.
        update.fields.status = None;
    }

    fn map_event(&self, event: AgentEvent) -> Option<SessionUpdate> {
        if self.replay {
            return agent_event_to_replay_update(event, &self.source_cluster);
        }
        if let Some(target) = committed_target(&event, &self.source_cluster) {
            if self
                .session
                .as_ref()
                .is_some_and(|session| !session.commit_handoff(target.clone()))
            {
                return None;
            }
            return Some(fallback_update(&target));
        }
        agent_event_to_session_update_for_cluster(event, &self.source_cluster)
    }
}

/// `map_event` projects events nested in `AgentEvent::SubAgent` wrappers, so
/// the terminal guard has to look through them too.
fn innermost_event(mut event: &AgentEvent) -> &AgentEvent {
    while let AgentEvent::SubAgent { event: inner, .. } = event {
        event = inner;
    }
    event
}

/// Tool call id a projected sub-agent progress update is keyed on, if `event` is one.
fn progress_update_id(event: &AgentEvent) -> Option<String> {
    let AgentEvent::Turn(harnx_core::event::TurnEvent::SubAgentProgress(progress)) =
        innermost_event(event)
    else {
        return None;
    };
    Some(
        progress
            .tool_call_id
            .clone()
            .unwrap_or_else(|| progress.invocation_id.clone()),
    )
}

impl AgentEventSink for AcpEventSink {
    fn emit(&self, event: AgentEvent) {
        self.record_terminal(&event);
        let progress_id = progress_update_id(&event);
        if let Some(mut update) = self.map_event(event) {
            self.strip_late_progress_status(progress_id.as_deref(), &mut update);
            let msg = AcpMessage::Update {
                session_id: self.session_id.clone(),
                update: Box::new(update),
            };
            // If send fails, the drain task has stopped (turn cancelled or similar)
            let _ = self.tx.send(msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::event::{ContentBlock, ModelEvent, SessionEvent, TurnEvent};

    #[test]
    fn sink_creates_channel() {
        let (sink, _rx) = AcpEventSink::new("test-session".to_string());
        assert_eq!(sink.session_id(), "test-session");
    }

    fn start_tool(sink: &AcpEventSink, rx: &mut mpsc::UnboundedReceiver<AcpMessage>, id: &str) {
        use harnx_core::event::{ToolEvent, ToolKind};
        sink.emit(AgentEvent::Tool(ToolEvent::Started {
            id: id.into(),
            name: "subagent".into(),
            kind: ToolKind::Other,
            markdown: None,
            input: serde_json::json!({}),
            locations: vec![],
        }));
        let _ = rx.try_recv().expect("tool start");
    }

    fn progress_event(tool_call_id: &str) -> AgentEvent {
        use harnx_core::api_types::CompletionTokenUsage;
        use harnx_core::event::{SubAgentProgress, SubAgentProgressStatus};
        AgentEvent::Turn(TurnEvent::SubAgentProgress(SubAgentProgress {
            invocation_id: format!("wire-{tool_call_id}"),
            agent: "atlas".into(),
            session_id: "child-session".into(),
            status: SubAgentProgressStatus::Done,
            elapsed_ms: 1000,
            usage: CompletionTokenUsage::default(),
            tool_call_count: 1,
            title: Some("Finished".into()),
            tool_call_id: Some(tool_call_id.into()),
        }))
    }

    fn emit_tool_call_update(
        sink: &AcpEventSink,
        rx: &mut mpsc::UnboundedReceiver<AcpMessage>,
        event: AgentEvent,
    ) -> agent_client_protocol::schema::v1::ToolCallUpdate {
        sink.emit(event);
        let AcpMessage::Update { update, .. } = rx.try_recv().expect("tool call update") else {
            panic!("expected update message");
        };
        let SessionUpdate::ToolCallUpdate(update) = *update else {
            panic!("expected tool call update");
        };
        update
    }

    #[test]
    fn terminal_calls_suppress_late_progress_status() {
        use harnx_core::event::ToolEvent;
        let (sink, mut rx) = AcpEventSink::new("test-session".to_string());
        let terminal_events = [
            (
                "completed-call",
                ToolEvent::Completed {
                    id: "completed-call".into(),
                    output: serde_json::json!("done"),
                    markdown: None,
                },
                agent_client_protocol::schema::v1::ToolCallStatus::Completed,
            ),
            (
                "failed-call",
                ToolEvent::Failed {
                    id: "failed-call".into(),
                    error: "failed".into(),
                },
                agent_client_protocol::schema::v1::ToolCallStatus::Failed,
            ),
        ];

        for (id, event, status) in terminal_events {
            start_tool(&sink, &mut rx, id);
            let terminal = emit_tool_call_update(&sink, &mut rx, AgentEvent::Tool(event));
            assert_eq!(terminal.fields.status, Some(status));
            let progress = emit_tool_call_update(&sink, &mut rx, progress_event(id));
            assert_eq!(progress.fields.status, None);
            assert!(
                progress.fields.title.is_some(),
                "late progress title is retained"
            );
        }
    }

    #[test]
    fn wrapped_late_progress_status_is_suppressed() {
        use harnx_core::event::{AgentSource, ToolEvent};
        let (sink, mut rx) = AcpEventSink::new("test-session".to_string());
        start_tool(&sink, &mut rx, "wrapped-call");
        emit_tool_call_update(
            &sink,
            &mut rx,
            AgentEvent::Tool(ToolEvent::Failed {
                id: "wrapped-call".into(),
                error: "failed".into(),
            }),
        );
        let wrapped = AgentEvent::SubAgent {
            source: AgentSource {
                agent: "child".into(),
                session_id: None,
                model: None,
            },
            event: Box::new(progress_event("wrapped-call")),
        };
        let progress = emit_tool_call_update(&sink, &mut rx, wrapped);
        assert_eq!(progress.fields.status, None);
    }

    #[test]
    fn progress_for_unrelated_ids_keeps_in_progress_status() {
        use harnx_core::event::ToolEvent;
        let (sink, mut rx) = AcpEventSink::new("test-session".to_string());
        start_tool(&sink, &mut rx, "terminal-call");
        start_tool(&sink, &mut rx, "live-call");
        let terminal = emit_tool_call_update(
            &sink,
            &mut rx,
            AgentEvent::Tool(ToolEvent::Completed {
                id: "terminal-call".into(),
                output: serde_json::json!("done"),
                markdown: None,
            }),
        );
        assert_eq!(
            terminal.fields.status,
            Some(agent_client_protocol::schema::v1::ToolCallStatus::Completed),
        );

        assert_eq!(
            emit_tool_call_update(&sink, &mut rx, progress_event("live-call"))
                .fields
                .status,
            Some(agent_client_protocol::schema::v1::ToolCallStatus::InProgress),
        );
        assert_eq!(
            emit_tool_call_update(&sink, &mut rx, progress_event("unrelated-call"))
                .fields
                .status,
            Some(agent_client_protocol::schema::v1::ToolCallStatus::InProgress),
        );
    }

    #[test]
    fn emit_message_chunk_sends_update() {
        let (sink, mut rx) = AcpEventSink::new("test-session".to_string());

        sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("hello".to_string())],
        }));

        let msg = rx.try_recv().expect("should have message");
        match msg {
            AcpMessage::Update { session_id, update } => {
                assert_eq!(session_id, "test-session");
                let SessionUpdate::AgentMessageChunk(chunk) = *update else {
                    panic!("expected agent message chunk");
                };
                let agent_client_protocol::schema::v1::ContentBlock::Text(text) = chunk.content
                else {
                    panic!("expected text content");
                };
                assert_eq!(text.text, "hello");
            }
            _ => panic!("expected Update message"),
        }
    }

    #[test]
    fn emit_error_sends_error_event() {
        let (sink, mut rx) = AcpEventSink::new("test-session".to_string());

        sink.emit(AgentEvent::Model(ModelEvent::Error(
            "something went wrong".to_string(),
        )));

        let msg = rx.try_recv().expect("should have message");
        match msg {
            AcpMessage::Update { session_id, update } => {
                assert_eq!(session_id, "test-session");
                let SessionUpdate::AgentMessageChunk(chunk) = *update else {
                    panic!("expected agent message chunk");
                };
                assert_eq!(
                    chunk
                        .meta
                        .and_then(|meta| meta.get(crate::HARNX_ERROR_META).cloned()),
                    Some(serde_json::Value::Bool(true))
                );
            }
            _ => panic!("expected Update message"),
        }
    }

    #[test]
    fn only_committed_handoff_deactivates_source_and_emits_fallback() {
        let session = Arc::new(SessionContext::new_for_test());
        let (sink, mut rx) = AcpEventSink::for_session(
            "source-session".to_string(),
            "source-cluster".to_string(),
            Arc::clone(&session),
        );

        sink.emit(AgentEvent::Turn(TurnEvent::HandoffRequested {
            agent: "atlas@prod".to_string(),
            session_id: Some("tentative".to_string()),
        }));
        assert!(session.handoff_target().is_none());
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        sink.emit(AgentEvent::Session(SessionEvent::HandoffCommitted {
            agent: "atlas@prod".to_string(),
            session_id: "target-session".to_string(),
            handoff_tool_call_id: Some("handoff-call".to_string()),
            after_seq: Some(42),
        }));
        let target = session.handoff_target().expect("source is inactive");
        assert_eq!(
            (target.cluster(), target.agent(), target.local_session_id()),
            ("prod", "atlas", "target-session")
        );
        assert!(matches!(rx.try_recv(), Ok(AcpMessage::Update { .. })));
    }

    #[test]
    fn signal_handle_emits_turn_complete() {
        let (sink, mut rx) = AcpEventSink::new("test-session".to_string());

        // Create signal handle before moving sink into Arc
        let handle = sink.signal_handle();

        // Simulate moving sink elsewhere (like Arc::new(sink))
        let _sink = sink;

        // Signal handle works independently
        handle.signal_complete();

        let msg = rx.try_recv().expect("should have message");
        match msg {
            AcpMessage::TurnComplete => {}
            _ => panic!("expected TurnComplete message"),
        }
    }

    #[test]
    fn emit_turn_complete_signals() {
        let (sink, mut rx) = AcpEventSink::new("test-session".to_string());

        // Signal via handle
        sink.signal_handle().signal_complete();

        let msg = rx.try_recv().expect("should have message");
        match msg {
            AcpMessage::TurnComplete => {}
            _ => panic!("expected TurnComplete message"),
        }
    }
}
