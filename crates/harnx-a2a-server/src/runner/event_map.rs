//! Only user-visible text crosses the A2A boundary. Terminal status comes from
//! the durable turn result, not a model round's Final event.
use a2a_lf::{Artifact, Message, Part, Role, StreamResponse, TaskState, TaskStatus};
use chrono::Utc;
use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent};
use tokio::sync::mpsc;

/// Revision belongs to the persisted task snapshot, not the wire payload.
#[derive(Debug, Clone)]
pub struct A2aEvent {
    pub revision: u64,
    pub response: StreamResponse,
}

impl A2aEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(&self.response, StreamResponse::StatusUpdate(event) if event.status.state.is_terminal())
    }
}

pub(super) struct A2aEventSink(pub mpsc::UnboundedSender<AgentEvent>);
impl harnx_core::event::AgentEventSink for A2aEventSink {
    fn emit(&self, event: AgentEvent) {
        // Persistence and broadcast are handled by one sequential consumer.
        if matches!(event, AgentEvent::Model(ModelEvent::MessageChunk { .. })) {
            let _ = self.0.send(event);
        }
    }
}

#[derive(Default)]
pub(super) struct Output {
    pub text: String,
    pub pending: String,
    pub sent: bool,
}
impl Output {
    pub fn accept(&mut self, event: AgentEvent) {
        if let AgentEvent::Model(ModelEvent::MessageChunk { blocks }) = event {
            for block in blocks {
                if let ContentBlock::Text(text) = block {
                    self.text.push_str(&text);
                    self.pending.push_str(&text);
                }
            }
        }
    }
}

pub(super) fn artifact(text: String) -> Artifact {
    Artifact {
        artifact_id: "answer".into(),
        name: None,
        description: None,
        parts: vec![Part::text(text)],
        metadata: None,
        extensions: None,
    }
}

pub(super) fn status(state: TaskState, text: Option<&str>) -> TaskStatus {
    TaskStatus {
        state,
        message: text.map(|text| Message::new(Role::Agent, vec![Part::text(text)])),
        timestamp: Some(Utc::now()),
    }
}

// Worker errors can contain credentials, URLs, prompts and filesystem paths.
// Keep details in server logs; never try to redact an arbitrary error string.
pub(super) const FAILED_MESSAGE: &str = "agent turn failed";
