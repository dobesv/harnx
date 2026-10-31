//! Informational replay messages whose identity must survive compaction.

use harnx_core::message::{Message, MessageContent, MessageRole};
pub use harnx_core::runtime_note::RUNTIME_NOTE_PREFIX;
use harnx_core::session::SessionLogEntry;

pub(super) enum RuntimeNote<'a> {
    SubAgentStarted { agent: &'a str, session_id: &'a str },
    Cancel,
}

impl<'a> RuntimeNote<'a> {
    pub(super) fn from_entry(entry: &'a SessionLogEntry) -> Option<Self> {
        match entry {
            SessionLogEntry::SubAgentStarted {
                agent, session_id, ..
            } => Some(Self::SubAgentStarted { agent, session_id }),
            SessionLogEntry::Cancel { .. } => Some(Self::Cancel),
            _ => None,
        }
    }

    pub(super) fn into_message(self, seq: usize) -> Message {
        let text = match self {
            Self::SubAgentStarted { agent, session_id } => format!(
                "{RUNTIME_NOTE_PREFIX}Started sub-agent '{agent}' in session {session_id}."
            ),
            Self::Cancel => format!(
                "{RUNTIME_NOTE_PREFIX}The user interrupted this turn. Incomplete tool calls above were cancelled."
            ),
        };
        // Compaction re-logs this id verbatim, keeping the note non-executable
        // instead of persisting an ID-less user row that admission rejects.
        Message::new(MessageRole::User, MessageContent::Text(text))
            .with_id(harnx_core::runtime_note::new_runtime_note_message_id())
            .with_log_seq(seq)
    }
}
