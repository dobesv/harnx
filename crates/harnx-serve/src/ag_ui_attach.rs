use ag_ui_core::{
    event::{BaseEvent, CustomEvent, Event, MessagesSnapshotEvent},
    types::message::Message as AgUiMessage,
};
use bytes::Bytes;
use serde_json::json;

use crate::ag_ui::frame_event;
use crate::ag_ui_sync::history_warning_event;

pub(crate) fn session_attach_boundary_event(attached_seq: u64) -> Event {
    Event::Custom(CustomEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        name: "session_attach_boundary".to_string(),
        value: json!({ "attached_seq": attached_seq }),
    })
}

pub(crate) fn session_attach_boundary_frame(attached_seq: u64) -> Bytes {
    Bytes::from(
        frame_event(&session_attach_boundary_event(attached_seq))
            .expect("session attach boundary should serialize"),
    )
}

pub(crate) fn snapshot_event(messages: Vec<AgUiMessage>) -> Event {
    Event::MessagesSnapshot(MessagesSnapshotEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        messages,
    })
}

pub(crate) fn initial_attach_frame(
    snapshot: Vec<AgUiMessage>,
    history_warnings: Vec<String>,
    include_snapshot: bool,
    additional_event: Option<Event>,
) -> Option<Bytes> {
    let frames = include_snapshot
        .then(|| snapshot_event(snapshot))
        .into_iter()
        .chain(history_warnings.into_iter().map(history_warning_event))
        .chain(additional_event)
        .filter_map(|event| {
            frame_event(&event)
                .map_err(|err| log::warn!("failed to serialize initial AG-UI frame: {err}"))
                .ok()
        })
        .collect::<String>();
    (!frames.is_empty()).then(|| Bytes::from(frames))
}

pub(crate) fn keep_alive_frame() -> &'static str {
    ": keep-alive\n\n"
}
