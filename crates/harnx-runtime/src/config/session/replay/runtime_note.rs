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

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::runtime_note::is_reserved_message_id;
    use harnx_core::session::SessionLogEntry;

    // Contract tests: the note wording here and the display helper in
    // harnx-core must stay in sync. If this wording drifts, compact_display
    // silently degrades to its truncated-first-line fallback, and these
    // tests are what catches it.

    #[test]
    fn sub_agent_note_carries_reserved_id_and_compacts_in_display() {
        let entry = SessionLogEntry::SubAgentStarted {
            agent: "pantheon/plato".to_string(),
            session_id: "child-direct".to_string(),
            invocation_id: None,
            tool_call_id: None,
            started_at: None,
        };
        let message = RuntimeNote::from_entry(&entry)
            .expect("sub-agent started entry synthesizes a note")
            .into_message(7);
        assert!(message.id.as_deref().is_some_and(is_reserved_message_id));
        assert_eq!(
            harnx_core::runtime_note::compact_display(&message.content.to_text()).as_deref(),
            Some("Sub-agent 'pantheon/plato' started (session child-direct)")
        );
    }

    #[test]
    fn cancel_note_carries_reserved_id_and_compacts_in_display() {
        let entry = SessionLogEntry::Cancel {
            fence_token: 0,
            cancellation_id: None,
            requested_by: None,
            timestamp: None,
        };
        let message = RuntimeNote::from_entry(&entry)
            .expect("cancel entry synthesizes a note")
            .into_message(9);
        assert!(message.id.as_deref().is_some_and(is_reserved_message_id));
        assert_eq!(
            harnx_core::runtime_note::compact_display(&message.content.to_text()).as_deref(),
            Some("Turn interrupted; incomplete tool calls cancelled")
        );
    }

    #[test]
    fn injected_compaction_summary_note_compacts_in_display() {
        // Contract test: the summary note that build_messages injects must
        // compact in the display helper. If the wording drifts,
        // compact_display degrades to its truncated-first-line fallback.
        let mut session =
            crate::config::session::new(&crate::config::Config::default(), "test", None).unwrap();
        crate::config::session::attach_memory_log(&mut session);
        session.compaction_summary = Some("PRIOR WORK SUMMARY".to_string());

        let input = crate::config::Input::new(
            "next question".to_string(),
            ("next question".to_string(), vec![]),
            harnx_core::agent_config::AgentConfig::from_prompt(""),
        );
        let messages = crate::config::session::build_messages(&session, &input).unwrap();
        let summary_row = messages
            .iter()
            .find(|m| m.content.to_text().starts_with(RUNTIME_NOTE_PREFIX))
            .expect("summary note row");
        assert_eq!(
            harnx_core::runtime_note::compact_display(&summary_row.content.to_text()).as_deref(),
            Some("Earlier conversation summary")
        );
    }
}
