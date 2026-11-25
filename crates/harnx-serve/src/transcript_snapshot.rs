//! AG-UI projection of the shared default transcript view.

use crate::history_projection::{project_message, HistoryReasoning};
use ag_ui_core::{
    event::{BaseEvent, CustomEvent, Event},
    types::message::Message as AgUiMessage,
};
use harnx_core::{
    message::Message,
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
    pub reasoning: HistoryReasoning,
    pub compaction: Option<SessionCompaction>,
}

impl From<Vec<AgUiMessage>> for HistorySnapshot {
    fn from(messages: Vec<AgUiMessage>) -> Self {
        Self {
            messages,
            ..Self::default()
        }
    }
}

impl HistorySnapshot {
    pub(crate) fn from_messages(history: &[Message]) -> Self {
        let mut snapshot = Self::from(Vec::with_capacity(history.len()));
        for (ordinal, message) in history.iter().enumerate() {
            let projected = project_message(message, ordinal);
            snapshot.messages.extend(projected.rows);
            snapshot.reasoning.extend(projected.reasoning);
        }
        snapshot
    }

    pub(crate) fn from_entries(
        entries: &[(u64, SessionLogEntry)],
        base: &Session,
        preserve_pending: bool,
    ) -> anyhow::Result<Self> {
        let view = default_view(entries)?;
        let session = replay_history(&view.entries, base, preserve_pending)?;
        Ok(Self::from_messages(&session.messages).with_compaction(view.compaction))
    }

    fn with_compaction(mut self, marker: Option<CompactionMarker>) -> Self {
        self.compaction = marker.map(SessionCompaction::from);
        if let Some(compaction) = &self.compaction {
            self.messages.insert(0, compaction.message());
        }
        self
    }

    /// Keep rows and reasoning in one queued event. A broadcast lag or remote
    /// sequence fence can drop an event; it must not separate these two frames.
    /// `SnapshotFeatures::expand_live_event` consumes the internal `raw_event`
    /// before framing the snapshot for a subscriber.
    pub(crate) fn live_events(self) -> [Event; 2] {
        let mut snapshot = crate::ag_ui_attach::messages_snapshot(self.messages);
        snapshot.base.raw_event = Some(json!(self.reasoning));
        [
            compaction_event(self.compaction.as_ref()),
            Event::MessagesSnapshot(snapshot),
        ]
    }

    pub(crate) fn events_for(self, features: SnapshotFeatures) -> Vec<Event> {
        let mut events = vec![
            compaction_event(self.compaction.as_ref()),
            crate::ag_ui_attach::snapshot_event(self.messages),
        ];
        if features.history_reasoning {
            events.push(history_reasoning_event(json!(self.reasoning)));
        }
        events
    }
}

fn replay_history(
    entries: &[(u64, SessionLogEntry)],
    base: &Session,
    preserve_pending: bool,
) -> anyhow::Result<Session> {
    use harnx_runtime::nats_session_log::{
        load_session_from_entries_with_metadata as replay,
        load_session_from_entries_with_metadata_preserving_pending as replay_active,
    };
    let replay = if preserve_pending {
        replay_active
    } else {
        replay
    };
    replay(entries, base.id(), base.clone())
}

#[cfg(test)]
pub(crate) fn history_messages_for_snapshot(history: &[Message]) -> Vec<AgUiMessage> {
    HistorySnapshot::from_messages(history).messages
}

const HISTORY_REASONING: &str = "history_reasoning";

/// `x-harnx-snapshot-features: history-reasoning` opts an SSE request into a
/// companion CUSTOM event immediately after each MESSAGES_SNAPSHOT.
#[derive(Clone, Copy, Default)]
pub(crate) struct SnapshotFeatures {
    history_reasoning: bool,
}

impl SnapshotFeatures {
    #[cfg(test)]
    pub(crate) const WITH_HISTORY_REASONING: Self = Self {
        history_reasoning: true,
    };

    pub(crate) fn from_headers(headers: &http::HeaderMap) -> Self {
        Self {
            history_reasoning: headers
                .get_all("x-harnx-snapshot-features")
                .iter()
                .filter_map(|value| value.to_str().ok())
                .flat_map(|value| value.split(','))
                .any(|token| token.trim() == "history-reasoning"),
        }
    }

    pub(crate) fn expand_live_event(self, mut event: Event) -> impl Iterator<Item = Event> {
        let reasoning = match &mut event {
            Event::MessagesSnapshot(snapshot) => snapshot.base.raw_event.take(),
            _ => None,
        };
        std::iter::once(event).chain(
            reasoning
                .filter(|_| self.history_reasoning)
                .map(history_reasoning_event),
        )
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

fn history_reasoning_event(reasoning: Value) -> Event {
    Event::Custom(CustomEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        name: HISTORY_REASONING.to_owned(),
        value: reasoning,
    })
}

#[cfg(test)]
pub(crate) mod tests;
