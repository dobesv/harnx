//! AG-UI projection of the shared default transcript view.

use ag_ui_core::{
    event::{BaseEvent, CustomEvent, Event},
    types::message::Message as AgUiMessage,
};
use harnx_core::{
    session::{Session, SessionLogEntry},
    transcript_view::{default_view, CompactionMarker},
};
use serde::Serialize;
use serde_json::{json, Value};

/// Display metadata linking a compaction marker to its synthetic AG-UI row.
/// Archive bounds are inclusive; `from_seq > to_seq` denotes an empty archive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionCompaction {
    pub message_id: String,
    pub compaction_seq: u64,
    pub summary: String,
    pub from_seq: u64,
    pub to_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub earlier_compaction: Option<u64>,
}

impl From<CompactionMarker> for SessionCompaction {
    fn from(marker: CompactionMarker) -> Self {
        Self {
            message_id: crate::ag_ui_sync::wire_message_id(&format!(
                "compaction:{}",
                marker.compaction_seq
            ))
            .to_string(),
            compaction_seq: marker.compaction_seq,
            summary: marker.summary,
            from_seq: marker.from_seq,
            to_seq: marker.to_seq,
            kept_count: marker.kept_count,
            earlier_compaction: marker.earlier_compaction,
        }
    }
}

impl SessionCompaction {
    pub(crate) fn message(&self) -> AgUiMessage {
        AgUiMessage::Assistant {
            id: self
                .message_id
                .parse()
                .expect("compaction message ID is a UUID"),
            content: Some(self.summary.clone()),
            name: None,
            tool_calls: None,
        }
    }

    pub(crate) fn extend_completed(&self, payload: &mut Value) {
        payload["summary"] = json!(self.summary);
        payload["from_seq"] = json!(self.from_seq);
        payload["to_seq"] = json!(self.to_seq);
        payload["compaction_seq"] = json!(self.compaction_seq);
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct HistorySnapshot {
    pub messages: Vec<AgUiMessage>,
    pub compaction: Option<SessionCompaction>,
}

impl From<Vec<AgUiMessage>> for HistorySnapshot {
    fn from(messages: Vec<AgUiMessage>) -> Self {
        Self {
            messages,
            compaction: None,
        }
    }
}

impl HistorySnapshot {
    pub(crate) fn from_entries(
        entries: &[(u64, SessionLogEntry)],
        base: &Session,
        preserve_pending: bool,
    ) -> anyhow::Result<Self> {
        let view = default_view(entries)?;
        let replay = if preserve_pending {
            harnx_runtime::nats_session_log::load_session_from_entries_with_metadata_preserving_pending
        } else {
            harnx_runtime::nats_session_log::load_session_from_entries_with_metadata
        };
        let session = replay(&view.entries, base.id(), base.clone())?;
        let mut snapshot = Self::from(crate::ag_ui::history_messages_for_snapshot(
            &session.messages,
        ));
        snapshot.compaction = view.compaction.map(SessionCompaction::from);
        if let Some(compaction) = &snapshot.compaction {
            snapshot.messages.insert(0, compaction.message());
        }
        Ok(snapshot)
    }

    pub(crate) fn events(self) -> Vec<Event> {
        snapshot_events(self.messages, self.compaction.as_ref())
    }
}

pub(crate) fn compaction_event(compaction: Option<&SessionCompaction>) -> Event {
    Event::Custom(CustomEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        name: "session_compaction".to_string(),
        value: json!({ "compaction": compaction }),
    })
}

pub(crate) fn snapshot_events(
    messages: Vec<AgUiMessage>,
    compaction: Option<&SessionCompaction>,
) -> Vec<Event> {
    vec![
        compaction_event(compaction),
        crate::ag_ui_attach::snapshot_event(messages),
    ]
}

#[cfg(test)]
pub(crate) mod tests;
