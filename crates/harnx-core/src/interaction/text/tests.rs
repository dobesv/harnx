//! Unit tests for the text fallback: rendering and the answer parser, and
//! their round-trip through the response contract.

mod graph;

use serde_json::{json, Map, Value};

use crate::interaction::response::build_data;
use crate::interaction::text::{parse, render, render_url};

/// The guide's deploy form (no context entries, exercising the plain path).
fn payload() -> Value {
    json!({
        "version": "1.0",
        "createSurface": {
            "dataModel": {"choice": [], "name": "rel-0"},
            "components": [
                {"id": "root", "component": "Card", "child": "col"},
                {"id": "col", "component": "Column",
                 "children": ["title", "choice", "name", "go_label", "go"]},
                {"id": "title", "component": "Text", "text": "Pick a deployment target"},
                {"id": "choice", "component": "ChoicePicker",
                 "options": [{"label": "Staging", "value": "staging"},
                             {"label": "Production", "value": "prod"}],
                 "value": {"@path": "/choice"}},
                {"id": "name", "component": "TextField", "label": "Release name",
                 "value": {"@path": "/name"}},
                {"id": "go_label", "component": "Text", "text": "Deploy"},
                {"id": "go", "component": "Button", "child": "go_label",
                 "action": {"event": {"name": "deploy"}}}
            ]
        }
    })
}

#[test]
fn render_shows_text_inputs_options_and_actions() {
    let text = render(&payload());
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.contains(&"Pick a deployment target"), "{text}");
    assert!(lines.contains(&"Release name: rel-0"), "{text}");
    assert!(text.contains("1. Staging [staging]"), "{text}");
    assert!(text.contains("2. Production [prod]"), "{text}");
    assert!(text.contains("Actions: deploy (Deploy), deny"), "{text}");
    assert!(text.contains("key=value"), "{text}");
}

#[test]
fn parse_builds_the_canonical_response() {
    let answer = parse(&payload(), "deploy choice=Staging name=rel-1").unwrap();
    assert_eq!(answer.action, "deploy");
    assert_eq!(
        answer.data["choice"],
        json!(["staging"]),
        "labels map to values"
    );
    assert_eq!(answer.data["name"], json!("rel-1"));
    assert_eq!(answer.note, None);
}

#[test]
fn parse_accepts_action_labels_pointers_and_deny() {
    assert_eq!(parse(&payload(), "Deploy").unwrap().action, "deploy");
    let answer = parse(&payload(), "deploy /name=rel-2").unwrap();
    assert_eq!(answer.data["name"], json!("rel-2"));
    let denied = parse(&payload(), "deny").unwrap();
    assert_eq!(denied.action, "deny");
    assert!(denied.data.is_empty());
}

#[test]
fn parse_rejects_bad_answers_with_actionable_errors() {
    for (answer, expected) in [
        ("", "empty answer"),
        ("publish", "names no action"),
        ("deploy name", "key=value"),
        ("deploy bogus=1", "unknown input key 'bogus'"),
        ("deploy choice=canary", "not a declared option"),
    ] {
        let error = parse(&payload(), answer).unwrap_err();
        assert!(error.contains(expected), "'{answer}': {error}");
    }
}

/// The round-trip frontends rely on: render, answer by text, and the parsed
/// response is the same one a graphical renderer builds and acceptance takes.
#[test]
fn text_answer_round_trips_through_the_response_contract() {
    let answer = parse(&payload(), "deploy choice=prod name=rel-1").unwrap();

    // The canonical builder produces the same data for the same input.
    let mut values = Map::new();
    values.insert("choice".into(), json!(["prod"]));
    values.insert("name".into(), json!("rel-1"));
    let built = build_data(&payload(), "deploy", &values).unwrap();
    assert_eq!(answer.data, built);

    // Acceptance validation takes it.
    assert_eq!(
        crate::interaction::response::validate_response(&payload(), &answer),
        Ok(())
    );
}

/// An untouched input defaults through the declared data model, matching the
/// builder, so a partial text answer and a graphical answer with an empty
/// field produce the same data.
#[test]
fn parse_defaults_untouched_inputs() {
    let answer = parse(&payload(), "deploy").unwrap();
    let built = build_data(&payload(), "deploy", &Map::new()).unwrap();
    assert_eq!(answer.data, built);
    assert_eq!(answer.data["choice"], json!([]));
    assert_eq!(answer.data["name"], json!("rel-0"));
}

#[test]
fn ambiguous_labels_ask_for_the_action_name() {
    let surface = json!({
        "version": "1.0",
        "createSurface": {"components": [
            {"id": "root", "component": "Column", "children": ["a_label", "a", "b_label", "b"]},
            {"id": "a_label", "component": "Text", "text": "Go"},
            {"id": "a", "component": "Button", "child": "a_label",
             "action": {"event": {"name": "first"}}},
            {"id": "b_label", "component": "Text", "text": "Go"},
            {"id": "b", "component": "Button", "child": "b_label",
             "action": {"event": {"name": "second"}}}
        ]}
    });
    let error = parse(&surface, "Go").unwrap_err();
    assert!(error.contains("matches 2 button labels"), "{error}");
    assert_eq!(parse(&surface, "first").unwrap().action, "first");
}

#[test]
fn render_and_parse_survive_an_unstamped_payload() {
    // Acceptance sees the stamped payload; renderers may render the raw
    // surface. Both shapes work identically here.
    let raw = payload();
    let rendered = render(&raw);
    assert!(rendered.contains("Pick a deployment target"));
    let answer = parse(&raw, "deploy choice=prod").unwrap();
    assert_eq!(answer.data["choice"], json!(["prod"]));
}

#[test]
fn render_redacts_obscured_defaults_without_hiding_other_inputs() {
    let mut form = payload();
    form["createSurface"]["dataModel"]["name"] = json!("OBSCURED_DEFAULT_SENTINEL");
    form["createSurface"]["components"][4]["variant"] = json!("obscured");
    form["createSurface"]["components"][1]["children"] = json!(["name", "public"]);
    form["createSurface"]["dataModel"]["public"] = json!("visible default");
    form["createSurface"]["components"].as_array_mut().unwrap().push(json!({
        "id": "public", "component": "TextField", "label": "Public", "value": {"@path": "/public"}
    }));
    let text = render(&form);
    assert!(text.contains("Release name: [redacted]"), "{text}");
    assert!(text.contains("Public: visible default"), "{text}");
    assert!(!text.contains("OBSCURED_DEFAULT_SENTINEL"), "{text}");
    assert!(!text.contains("/name"), "{text}");
}

#[test]
fn render_obscured_fields_do_not_disclose_presence_or_length_of_defaults() {
    let mut form = payload();
    form["createSurface"]["components"][4]["variant"] = json!("obscured");
    let expected = render(&form);
    for value in [
        json!(""),
        json!("longer private default"),
        Value::Null,
        json!(42),
    ] {
        form["createSurface"]["dataModel"]["name"] = value;
        assert_eq!(render(&form), expected);
    }
    form["createSurface"]["dataModel"]
        .as_object_mut()
        .unwrap()
        .remove("name");
    assert_eq!(render(&form), expected);
}

fn dynamic_payload() -> Value {
    let mut form = payload();
    form["createSurface"]["dataModel"]["labels"] = json!({
        "title": "Bound title", "choice": "Target", "option": "Canary", "name": "Release", "action": "Launch"
    });
    for (index, property, label) in [
        (2, "text", "title"),
        (3, "label", "choice"),
        (4, "label", "name"),
        (5, "text", "action"),
    ] {
        form["createSurface"]["components"][index][property] =
            json!({"@path": format!("/labels/{label}")});
    }
    form["createSurface"]["components"][3]["options"][0]["label"] =
        json!({"@path": "/labels/option"});
    form
}

#[test]
fn render_resolves_dynamic_text_and_input_labels() {
    let text = render(&dynamic_payload());
    for expected in [
        "Bound title",
        "Target:",
        "1. Canary [staging]",
        "Release: rel-0",
    ] {
        assert!(text.contains(expected), "{text}");
    }
}

#[test]
fn render_missing_or_non_string_dynamic_bindings_use_fallbacks() {
    let mut form = dynamic_payload();
    // All label bindings are now missing except a non-string ChoicePicker label.
    form["createSurface"]["dataModel"]["labels"] = json!({"choice": 42});
    let text = render(&form);
    assert!(text.contains("1. staging [staging]"), "{text}");
    assert!(text.contains("(input): rel-0"), "{text}");
    assert!(!text.contains("Pick a deployment target"), "{text}");
    assert!(!text.contains("42"), "{text}");
    assert!(!text.contains("@path"), "{text}");
}

#[test]
fn resolved_dynamic_action_and_option_labels_remain_answerable() {
    let form = dynamic_payload();
    let text = render(&form);
    assert!(text.contains("Actions: deploy (Launch), deny"), "{text}");
    assert!(text.contains("1. Canary [staging]"), "{text}");
    let fields = crate::interaction::response::input_fields(&form);
    assert_eq!(fields[0].label.as_deref(), Some("Target"));
    assert_eq!(fields[1].label.as_deref(), Some("Release"));
    let answer = parse(&form, "Launch choice=Canary").unwrap();
    assert_eq!(answer.action, "deploy");
    assert_eq!(answer.data["choice"], json!(["staging"]));
}

#[test]
fn render_url_prints_link_and_how_to_answer() {
    let payload = json!({
        "url": "https://example.com/connect/abc",
        "message": "Connect Jira"
    });
    let text = render_url(&payload);
    assert!(text.contains("Connect Jira"), "{text}");
    assert!(text.contains("https://example.com/connect/abc"), "{text}");
    assert!(text.contains("answer `continue`"), "{text}");
    assert!(text.contains("`cancel`"), "{text}");
    // A bare payload without a message still prints the link.
    let bare = json!({ "url": "http://127.0.0.1:8931/connect/abc" });
    let text = render_url(&bare);
    assert!(text.contains("http://127.0.0.1:8931/connect/abc"), "{text}");
    assert!(!text.starts_with("\n"), "{text}");
}
