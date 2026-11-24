use super::*;
use harnx_core::session::ToolOutput;
use serde_json::json;

fn surface_payload(id: &str) -> Value {
    json!({
        "version": "1.0",
        "createSurface": {
            "surfaceId": id,
            "components": [
                { "id": "title", "component": "Text", "text": "Deploy?" },
                { "id": "go", "component": "Button", "text": "Go",
                  "action": { "event": { "name": "deploy" } } }
            ]
        }
    })
}

fn results_entry(seq: u64, surfaces: Vec<Value>) -> (u64, SessionLogEntry) {
    (
        seq,
        SessionLogEntry::ToolResults {
            results: vec![ToolOutput {
                id: Some("call-1".into()),
                name: "deployer".into(),
                output: json!({}),
                markdown: None,
                content: Vec::new(),
                user_surfaces: surfaces,
                switch_agent: None,
            }],
            handoff_selection: None,
            timestamp: None,
        },
    )
}

#[test]
fn emits_one_event_per_published_surface_with_marker_identity() {
    let entries = vec![
        results_entry(7, vec![surface_payload("s-1")]),
        results_entry(8, vec![]),
    ];
    let events = surface_snapshot_events(&entries);
    assert_eq!(events.len(), 1);
    let Event::Custom(CustomEvent { name, value, .. }) = &events[0] else {
        panic!("expected custom event");
    };
    assert_eq!(name, "a2ui_surface");
    assert_eq!(value["surface_id"], "s-1");
    assert_eq!(value["publish_seq"], 7);
    assert_eq!(value["tool_call_id"], "call-1");
    assert_eq!(value["payload"]["createSurface"]["surfaceId"], "s-1");
}

#[test]
fn emits_resolution_events_for_accepted_actions() {
    let entries = vec![
        results_entry(7, vec![surface_payload("s-1")]),
        (
            9,
            SessionLogEntry::SurfaceAction {
                surface_id: "s-1".into(),
                publish_seq: 7,
                action: "deploy".into(),
                data: json!({}),
                note: None,
                user_message: None,
                visibility: "note".into(),
                note_text: Some("deployed".into()),
                fence_token: 1,
            },
        ),
    ];
    let events = surface_snapshot_events(&entries);
    assert_eq!(events.len(), 2);
    let Event::Custom(CustomEvent { value, .. }) = &events[1] else {
        panic!("expected custom event");
    };
    assert_eq!(value["action"], "deploy");
    assert_eq!(value["visibility"], "note");
    assert_eq!(value["note_text"], "deployed");
    // Marker identity dedupes against the publication event.
    assert_eq!(value["surface_id"], "s-1");
    assert_eq!(value["publish_seq"], 7);
}

#[test]
fn legacy_entries_emit_nothing() {
    let entries = vec![(
        1,
        SessionLogEntry::Message {
            id: None,
            role: harnx_core::message::MessageRole::User,
            content: harnx_core::message::MessageContent::Text("hi".into()),
            timestamp: None,
            fence_token: None,
        },
    )];
    assert!(surface_snapshot_events(&entries).is_empty());
}
