//! Only user-visible text crosses the A2A boundary. Terminal status comes from
//! the durable turn result, not a model round's Final event.
use a2a_lf::{Artifact, Message, Part, Role, StreamResponse, TaskState, TaskStatus};
use chrono::Utc;
use std::sync::Arc;

/// Stream cursor stays internal; wire responses contain only protocol fields.
#[derive(Debug, Clone)]
pub struct A2aEvent {
    /// Monotonic per-turn cursor, independent of task and bucket KV revisions.
    pub sequence: u64,
    pub response: StreamResponse,
}

impl A2aEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(&self.response, StreamResponse::StatusUpdate(event) if event.status.state.is_terminal())
    }
}

pub(super) struct A2aEventSink(pub Arc<super::inbox::TextInbox>);

#[derive(Default)]
pub(super) struct Output {
    pub text: String,
    pub pending: String,
    pub sent: bool,
}
impl Output {
    pub fn accept(&mut self, text: String, limit: usize) -> anyhow::Result<()> {
        let limit = if limit == 0 { 1024 * 1024 } else { limit };
        anyhow::ensure!(
            super::limits::encoded_len(&text)
                <= limit.saturating_sub(super::limits::encoded_len(&self.text)),
            "task output exceeds NATS payload budget"
        );
        self.text.push_str(&text);
        self.pending.push_str(&text);
        Ok(())
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
