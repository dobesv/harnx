//! Native confirm projection; durable manifests, not advisory requests, control visibility.

use ag_ui_core::event::{BaseEvent, CustomEvent, Event};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::interactions::derive_pending_interactions;
use serde_json::{json, Value};

pub(crate) fn pending_value(entries: &[(u64, SessionLogEntry)]) -> Option<Value> {
    let pending = match derive_pending_interactions(entries) {
        Ok(pending) => pending,
        Err(error) => {
            log::warn!("cannot derive pending interactions: {error:#}");
            return None;
        }
    };
    let first = pending.first()?;
    let mut val = json!({
        "interaction_id": first.interaction_id,
        "request_seq": first.request_seq,
        "tool_call_id": first.tool_call_id,
        "kind": first.request.kind,
        "message": first.summary(),
        "pending_count": pending.len() - 1,
    });
    if first.request.kind == "a2ui" && !first.request.payload.is_null() {
        val["payload"] = first.request.payload.clone();
    }
    // `url` challenges keep the link in the payload (`{url, message}`, built
    // by the runtime's tool-challenge middleware); frontends render the link
    // from it (decision 6).
    if first.request.kind == "url" && !first.request.payload.is_null() {
        val["payload"] = first.request.payload.clone();
    }
    Some(val)
}

/// A null snapshot clears a previously displayed request after acceptance or cancellation.
/// Legacy-only histories emit nothing, keeping their event contract unchanged.
pub(crate) fn pending_event(entries: &[(u64, SessionLogEntry)]) -> Option<Event> {
    if !entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::SuspensionReady { .. }))
    {
        return None;
    }
    Some(Event::Custom(CustomEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        name: "pending_interaction".into(),
        value: pending_value(entries).unwrap_or(Value::Null),
    }))
}

pub(crate) fn pending_outcome(entries: &[(u64, SessionLogEntry)]) -> Option<Value> {
    pending_value(entries).map(|pending| json!({ "type": "interaction", "pending": pending }))
}

#[cfg(test)]
pub(crate) mod tests;
