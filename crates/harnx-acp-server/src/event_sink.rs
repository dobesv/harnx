//! ACP event sink implementing AgentEventSink for in-order streaming.
//!
//! This implements the "single sequential drain loop" pattern from PR #1038:
//! NO per-chunk tokio::spawn. Instead, we use an unbounded channel to collect
//! events and a single drain task that sends session/update notifications in order.
//!
//! Key invariant: Drop the sink sender and await the drain task before
//! finishing the prompt turn to ensure all chunks flush in order.

use agent_client_protocol::schema::v1::SessionUpdate;
use harnx_core::event::{AgentEvent, AgentEventSink};
use tokio::sync::mpsc;

use crate::event_map::agent_event_to_session_update;

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
}

impl AcpEventSink {
    /// Create a new event sink for an ACP session.
    ///
    /// The sender should drop the sink and await the drain task to flush
    /// all pending updates before completing the prompt turn.
    pub fn new(session_id: String) -> (Self, mpsc::UnboundedReceiver<AcpMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx, session_id }, rx)
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
}

impl AgentEventSink for AcpEventSink {
    fn emit(&self, event: AgentEvent) {
        if let Some(update) = agent_event_to_session_update(event) {
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
    use harnx_core::event::{ContentBlock, ModelEvent};

    #[test]
    fn sink_creates_channel() {
        let (sink, _rx) = AcpEventSink::new("test-session".to_string());
        assert_eq!(sink.session_id(), "test-session");
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
