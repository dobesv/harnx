//! Tests for `harnx surface` (plan T5.1 Part B): text rendering, exact
//! identity resolution, and response building through the shared contract.

use super::*;
use act::build_surface_response;
use harnx_core::session::{SessionLogEntry, ToolOutput};
use serde_json::{json, Value};

/// A surface envelope with one text field, one choice picker and two declared
/// buttons, shaped like the Part A integration fixtures.
fn surface_envelope(id: &str) -> Value {
    json!({
        "version": "1.0",
        "createSurface": {
            "surfaceId": id,
            "components": [
                { "id": "root", "component": "Card", "child": "col" },
                { "id": "col", "component": "Column", "children": [
                    "title", "name_label", "name", "env_label", "env",
                    "ship_label", "ship"
                ] },
                { "id": "title", "component": "Text", "text": "Deployment ready" },
                { "id": "name_label", "component": "Text", "text": "Release name" },
                { "id": "name", "component": "TextField", "label": "Release name",
                  "value": { "@path": "/name" } },
                { "id": "env_label", "component": "Text", "text": "Environment" },
                { "id": "env", "component": "ChoicePicker",
                  "options": [
                    { "label": "Production", "value": "prod" },
                    { "label": "Staging", "value": "staging" }
                  ], "value": { "@path": "/env" } },
                { "id": "ship_label", "component": "Text", "text": "Ship it" },
                { "id": "ship", "component": "Button", "child": "ship_label",
                  "action": { "event": { "name": "ship" } } },
                { "id": "hold_label", "component": "Text", "text": "Hold" },
                { "id": "hold", "component": "Button", "child": "hold_label",
                  "action": { "event": { "name": "hold" } } }
            ]
        }
    })
}

fn publish_entry(seq: u64, payload: &Value) -> (u64, SessionLogEntry) {
    (
        seq,
        SessionLogEntry::ToolResults {
            results: vec![ToolOutput {
                id: Some("tool-1".into()),
                name: "publish".into(),
                output: json!({}),
                markdown: None,
                content: Vec::new(),
                user_surfaces: vec![payload.clone()],
                switch_agent: None,
            }],
            handoff_selection: None,
            timestamp: None,
        },
    )
}

fn entries_with_surfaces() -> Vec<(u64, SessionLogEntry)> {
    vec![
        publish_entry(3, &surface_envelope("deploy-1")),
        publish_entry(7, &surface_envelope("deploy-1")),
    ]
}

#[test]
fn surface_text_shows_identity_and_status() {
    let published = harnx_runtime::surfaces::published_surfaces(&entries_with_surfaces())
        .expect("surfaces derive");
    let (seq, surface) = &published[0];
    let outstanding = surface_text(*seq, surface, None);
    assert!(outstanding.contains("deploy-1"), "{outstanding}");
    assert!(
        outstanding.contains("published at sequence 3"),
        "{outstanding}"
    );
    assert!(outstanding.contains("outstanding"), "{outstanding}");
    assert!(outstanding.contains("Deployment ready"), "{outstanding}");
    assert!(outstanding.contains("Ship it"), "{outstanding}");

    let answered = surface_text(*seq, surface, Some("ship"));
    assert!(
        answered.contains("answered: ship (first-wins)"),
        "{answered}"
    );
}

#[test]
fn resolve_target_requires_exact_identity() {
    let entries = entries_with_surfaces();
    // Unique publications resolve without --publish-seq.
    let (seq, target) = resolve_target(&entries, "deploy-1", Some(3)).expect("seq resolves");
    assert_eq!(seq, 3);
    assert!(target.payload.pointer("/createSurface").is_some());

    // Two publications of the same id require --publish-seq.
    let error = resolve_target(&entries, "deploy-1", None).expect_err("ambiguous");
    assert!(error.to_string().contains("2 publications"), "{error}");

    // Unknown surface id is rejected, not defaulted.
    let error = resolve_target(&entries, "nope", None).expect_err("unknown");
    assert!(error.to_string().contains("not published"), "{error}");

    // Wrong sequence for a known id is rejected.
    let error = resolve_target(&entries, "deploy-1", Some(99)).expect_err("wrong seq");
    assert!(error.to_string().contains("matches none"), "{error}");
}

#[test]
fn build_surface_response_binds_inputs_via_shared_contract() {
    let payload = surface_envelope("deploy-1");
    let response = build_surface_response(
        &payload,
        "ship",
        &["name=rel-9".to_string(), "env=prod".to_string()],
        None,
    )
    .expect("response builds");
    assert_eq!(response.action, "ship");
    assert_eq!(response.note, None);
    assert_eq!(
        response.data.get("name").and_then(Value::as_str),
        Some("rel-9")
    );
    assert_eq!(
        response.data.get("env").and_then(Value::as_array),
        Some(&vec![json!("prod")])
    );

    // Labels map to option values through the shared grammar.
    let response = build_surface_response(&payload, "ship", &["env=Production".to_string()], None)
        .expect("label resolves");
    assert_eq!(
        response.data.get("env").and_then(Value::as_array),
        Some(&vec![json!("prod")])
    );

    // --data-json accepts a keyed object.
    let response = build_surface_response(&payload, "ship", &[], Some(r#"{"name": "rel-2"}"#))
        .expect("json data builds");
    assert_eq!(
        response.data.get("name").and_then(Value::as_str),
        Some("rel-2")
    );

    // Unknown input keys and undeclared actions are rejected before submit.
    let error = build_surface_response(&payload, "ship", &["bogus=1".to_string()], None)
        .expect_err("unknown key");
    assert!(error.to_string().contains("unknown input key"), "{error}");
    let error =
        build_surface_response(&payload, "self_destruct", &[], None).expect_err("undeclared");
    assert!(error.to_string().contains("not declared"), "{error}");
}
