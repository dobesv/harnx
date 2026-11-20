//! The published surface envelope and the publisher tool's output: the
//! user-audience embedded resources every visibility test acts on.

use serde_json::{json, Value};

/// The published surface envelope: one Button per visibility plus a
/// default-visibility direct tool target, an unauthorized tool target and an
/// invalid visibility — every outcome exercised by the tests below.
fn surface_envelope() -> Value {
    json!({
        "version": "1.0",
        "createSurface": {
            "surfaceId": "deploy-1",
            "components": [
                { "id": "root", "component": "Card", "child": "col" },
                { "id": "col", "component": "Column", "children": [
                    "title", "note_label", "note", "turn_label", "turn",
                    "invisible_label", "invisible", "tool_label", "tool",
                    "unauth_label", "unauth", "loud_label", "loud",
                    "slow_label", "slow", "turn_tool_label", "turn_tool",
                    "useronly_label", "useronly"
                ] },
                { "id": "title", "component": "Text", "text": "Deployment ready" },
                { "id": "note_label", "component": "Text", "text": "Note it" },
                { "id": "note", "component": "Button", "child": "note_label",
                  "action": { "event": { "name": "note_act" } },
                  "metadata": { "extensions": { "harnx": { "visibility": "note" } } } },
                { "id": "turn_label", "component": "Text", "text": "Discuss" },
                { "id": "turn", "component": "Button", "child": "turn_label",
                  "action": { "event": { "name": "turn_act" } },
                  "metadata": { "extensions": { "harnx": { "visibility": "turn" } } } },
                { "id": "invisible_label", "component": "Text", "text": "Quiet" },
                { "id": "invisible", "component": "Button", "child": "invisible_label",
                  "action": { "event": { "name": "invisible_act" } },
                  "metadata": { "extensions": { "harnx": { "visibility": "invisible" } } } },
                { "id": "tool_label", "component": "Text", "text": "Bump" },
                { "id": "tool", "component": "Button", "child": "tool_label",
                  "action": { "event": { "name": "tool_act" } },
                  "metadata": { "extensions": { "harnx": {
                      "tool": { "name": "surfaces_counter", "arguments": { "bump": "yes" } }
                  } } } },
                { "id": "unauth_label", "component": "Text", "text": "Nope" },
                { "id": "unauth", "component": "Button", "child": "unauth_label",
                  "action": { "event": { "name": "unauth_act" } },
                  "metadata": { "extensions": { "harnx": {
                      "tool": { "name": "surfaces_missing" }
                  } } } },
                { "id": "loud_label", "component": "Text", "text": "Loud" },
                { "id": "loud", "component": "Button", "child": "loud_label",
                  "action": { "event": { "name": "loud_act" } },
                  "metadata": { "extensions": { "harnx": { "visibility": "loud" } } } },
                { "id": "slow_label", "component": "Text", "text": "Slow" },
                { "id": "slow", "component": "Button", "child": "slow_label",
                  "action": { "event": { "name": "slow_act" } },
                  "metadata": { "extensions": { "harnx": {
                      "tool": { "name": "surfaces_slow" }
                  } } } },
                { "id": "turn_tool_label", "component": "Text", "text": "Slow turn" },
                { "id": "turn_tool", "component": "Button", "child": "turn_tool_label",
                  "action": { "event": { "name": "turn_tool_act" } },
                  "metadata": { "extensions": { "harnx": {
                      "visibility": "turn",
                      "tool": { "name": "surfaces_slow" }
                  } } } },
                { "id": "useronly_label", "component": "Text", "text": "Preview" },
                { "id": "useronly", "component": "Button", "child": "useronly_label",
                  "action": { "event": { "name": "useronly_act" } },
                  "metadata": { "extensions": { "harnx": {
                      "tool": { "name": "surfaces_useronly" }
                  } } } }
            ]
        }
    })
}

/// The publisher tool's output: model-visible text plus the user-audience
/// embedded resources carrying the surface envelopes (the T4.0 pin). Two
/// generations publish so a test can act on two surfaces without tripping
/// first-wins on one generation.
pub(super) fn publisher_output() -> Value {
    json!({
        "content": [
            { "type": "text", "text": "prepared" },
            { "type": "resource",
              "resource": {
                  "uri": "a2ui://deploy-1",
                  "mimeType": "application/a2ui+json",
                  "text": serde_json::to_string(&surface_envelope()).unwrap()
              },
              "annotations": { "audience": ["user"] } },
            { "type": "resource",
              "resource": {
                  "uri": "a2ui://note-2",
                  "mimeType": "application/a2ui+json",
                  "text": serde_json::to_string(&json!({
                      "version": "1.0",
                      "createSurface": {
                          "surfaceId": "note-2",
                          "components": [
                              { "id": "root", "component": "Card", "child": "col" },
                              { "id": "col", "component": "Column", "children": ["title", "note_label", "note"] },
                              { "id": "title", "component": "Text", "text": "Second surface" },
                              { "id": "note_label", "component": "Text", "text": "Note it too" },
                              { "id": "note", "component": "Button", "child": "note_label",
                                "action": { "event": { "name": "note_act" } },
                                "metadata": { "extensions": { "harnx": { "visibility": "note" } } } }
                          ]
                      }
                  })).unwrap()
              },
              "annotations": { "audience": ["user"] } }
        ]
    })
}
