//! `a2ui_surface` custom events for promptless attach (plan T5.1).
//!
//! Surfaces are durable: every user-audience envelope published by a
//! `ToolResults` entry hydrates from the log on attach, and an accepted action
//! re-emits the same event carrying the resolution. Marker identity is
//! `surface_id` + `publish_seq`, so hydrated and live emissions dedupe.

use ag_ui_core::event::{BaseEvent, CustomEvent, Event};
use harnx_core::session::SessionLogEntry;
use serde_json::{json, Value};

/// One `a2ui_surface` event per published surface, plus one per accepted
/// action (the same event carrying the resolution). Entries without surfaces
/// emit nothing, keeping legacy histories' event contract unchanged.
pub(crate) fn surface_snapshot_events(entries: &[(u64, SessionLogEntry)]) -> Vec<Event> {
    let mut events = Vec::new();
    for (seq, entry) in entries {
        match entry {
            SessionLogEntry::ToolResults { results, .. } => {
                events.extend(tool_results_surface_events(results, *seq));
            }
            SessionLogEntry::SurfaceAction {
                surface_id,
                publish_seq,
                action,
                visibility,
                note_text,
                ..
            } => {
                let event = surface_resolution_event(
                    surface_id,
                    *publish_seq,
                    action,
                    visibility,
                    note_text.as_deref(),
                );
                events.push(event);
            }
            _ => {}
        }
    }
    events
}

/// The `a2ui_surface` events for every surface one tool's outputs publish.
fn tool_results_surface_events(
    results: &[harnx_core::session::ToolOutput],
    seq: u64,
) -> Vec<Event> {
    let mut events = Vec::new();
    for output in results {
        for payload in &output.user_surfaces {
            if let Some(event) = surface_publish_event(payload, output.id.as_deref(), seq) {
                events.push(event);
            }
        }
    }
    events
}

/// The `a2ui_surface` event for one published surface, or `None` when the
/// envelope names no surface id (nothing a client could act on).
fn surface_publish_event(payload: &Value, tool_call_id: Option<&str>, seq: u64) -> Option<Event> {
    let surface_id = surface_id_of(payload)?;
    let mut value = json!({
        "surface_id": surface_id,
        "publish_seq": seq,
        "payload": payload,
    });
    if let Some(tool_call_id) = tool_call_id {
        value["tool_call_id"] = json!(tool_call_id);
    }
    Some(a2ui_surface_event(value))
}

/// The `a2ui_surface` event carrying one accepted action's resolution.
fn surface_resolution_event(
    surface_id: &str,
    publish_seq: u64,
    action: &str,
    visibility: &str,
    note_text: Option<&str>,
) -> Event {
    let mut resolution = json!({
        "surface_id": surface_id,
        "publish_seq": publish_seq,
        "action": action,
        "visibility": visibility,
    });
    if let Some(note_text) = note_text {
        resolution["note_text"] = json!(note_text);
    }
    a2ui_surface_event(resolution)
}

fn a2ui_surface_event(value: Value) -> Event {
    Event::Custom(CustomEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        name: "a2ui_surface".to_string(),
        value,
    })
}

fn surface_id_of(payload: &Value) -> Option<String> {
    payload
        .pointer("/createSurface/surfaceId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
pub(crate) mod tests;
