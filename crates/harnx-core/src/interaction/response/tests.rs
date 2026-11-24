//! Unit tests for the `a2ui` response contract: inspection, the canonical
//! `data` mapping, and acceptance validation.

use serde_json::{json, Map, Value};

use crate::interaction::response::{
    action_is_declared, build_data, find_action, input_fields, validate_response, InputKind,
};
use crate::interaction::InteractionResponse;

/// The guide's deploy form: two inputs, one action with a context that mixes
/// a bound input, a literal and a data-model-only path.
fn payload() -> Value {
    json!({
        "version": "1.0",
        "createSurface": {
            "dataModel": {"choice": [], "name": "", "region": "eu"},
            "components": [
                {"id": "root", "component": "Card", "child": "col"},
                {"id": "col", "component": "Column",
                 "children": ["title", "choice", "name", "submit_label", "submit"]},
                {"id": "title", "component": "Text", "text": "Pick a deployment target"},
                {"id": "choice", "component": "ChoicePicker", "variant": "mutuallyExclusive",
                 "options": [{"label": "Staging", "value": "staging"},
                             {"label": "Production", "value": "prod"}],
                 "value": {"@path": "/choice"}},
                {"id": "name", "component": "TextField", "label": "Release name",
                 "value": {"@path": "/name"}},
                {"id": "submit_label", "component": "Text", "text": "Deploy"},
                {"id": "submit", "component": "Button", "child": "submit_label",
                 "action": {"event": {"name": "deploy",
                    "context": {"choice": {"@path": "/choice"},
                                "mode": "fast",
                                "region": {"@path": "/region"}}}}}
            ]
        }
    })
}

fn response(action: &str, data: Value) -> InteractionResponse {
    InteractionResponse {
        action: action.into(),
        data: data.as_object().cloned().unwrap_or_default(),
        note: None,
    }
}

#[test]
fn inspection_exposes_inputs_and_actions() {
    let fields = input_fields(&payload());
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].key, "choice");
    assert_eq!(fields[0].kind, InputKind::ChoicePicker);
    assert_eq!(fields[0].options.len(), 2);
    assert!(fields[0].exclusive);
    assert_eq!(fields[1].key, "name");
    assert_eq!(fields[1].kind, InputKind::TextField);

    let deploy = find_action(&payload(), "deploy").expect("declared");
    assert_eq!(deploy.label.as_deref(), Some("Deploy"));
    assert_eq!(deploy.context.len(), 3);

    assert!(action_is_declared(&payload(), "deploy"));
    assert!(!action_is_declared(&payload(), "publish"));
    assert!(!action_is_declared(&payload(), "deny"));
}

#[test]
fn build_data_maps_inputs_and_resolves_action_context() {
    let mut values = Map::new();
    values.insert("choice".into(), json!(["prod"]));
    values.insert("name".into(), json!("rel-1"));
    let data = build_data(&payload(), "deploy", &values).unwrap();

    // Bound inputs keyed by their pointer minus the leading '/'.
    assert_eq!(data["choice"], json!(["prod"]));
    assert_eq!(data["name"], json!("rel-1"));
    // Context resolution: bound path -> the input's current value, literal
    // -> unchanged, data-model-only path -> its declared value.
    assert_eq!(
        data["choice"],
        json!(["prod"]),
        "context overlays the input key"
    );
    assert_eq!(data["mode"], json!("fast"));
    assert_eq!(data["region"], json!("eu"));
}

#[test]
fn build_data_defaults_untouched_inputs_from_the_data_model() {
    let values = Map::new();
    let data = build_data(&payload(), "deploy", &values).unwrap();
    assert_eq!(data["choice"], json!([]));
    assert_eq!(data["name"], json!(""));
}

#[test]
fn build_data_deny_carries_nothing_and_undeclared_fails() {
    let mut values = Map::new();
    values.insert("name".into(), json!("ignored"));
    assert!(build_data(&payload(), "deny", &values).unwrap().is_empty());
    let error = build_data(&payload(), "publish", &values).unwrap_err();
    assert!(error.contains("not declared"), "{error}");
}

#[test]
fn validate_accepts_declared_action_with_consistent_data() {
    let valid = response("deploy", json!({"choice": ["staging"], "name": "rel-1"}));
    assert_eq!(validate_response(&payload(), &valid), Ok(()));

    // Missing keys are allowed; context keys accept any JSON value.
    assert_eq!(
        validate_response(&payload(), &response("deploy", json!({}))),
        Ok(())
    );
    assert_eq!(
        validate_response(
            &payload(),
            &response("deploy", json!({"mode": {"nested": 1}}))
        ),
        Ok(())
    );

    // Deny is dismissal: always accepted, whatever a frontend echoes.
    assert_eq!(
        validate_response(&payload(), &response("deny", json!({"anything": 1}))),
        Ok(())
    );
}

#[test]
fn validate_rejects_undeclared_action() {
    let error = validate_response(&payload(), &response("publish", json!({}))).unwrap_err();
    assert!(error.contains("not declared"), "{error}");
}

#[test]
fn validate_rejects_wrong_data_shapes() {
    for (name, data, expected) in [
        (
            "unknown key",
            json!({"bogus": 1}),
            "matches no input binding",
        ),
        ("wrong type", json!({"name": 7}), "must be a string"),
        (
            "not an array",
            json!({"choice": "staging"}),
            "must be an array",
        ),
        (
            "undeclared option",
            json!({"choice": ["canary"]}),
            "undeclared option value 'canary'",
        ),
        (
            "not exclusive",
            json!({"choice": ["staging", "prod"]}),
            "mutually exclusive",
        ),
    ] {
        let Err(error) = validate_response(&payload(), &response("deploy", data)) else {
            panic!("expected rejection: {name}")
        };
        assert!(error.contains(expected), "{name}: {error}");
    }
}

#[test]
fn multiple_selection_pickers_accept_more_than_one_option() {
    let surface = json!({
        "version": "1.0",
        "createSurface": {"components": [
            {"id": "root", "component": "Column", "children": ["pick", "go_label", "go"]},
            {"id": "pick", "component": "ChoicePicker", "variant": "multipleSelection",
             "options": [{"label": "A", "value": "a"}, {"label": "B", "value": "b"}],
             "value": {"@path": "/picks"}},
            {"id": "go_label", "component": "Text", "text": "Go"},
            {"id": "go", "component": "Button", "child": "go_label",
             "action": {"event": {"name": "go"}}}
        ]}
    });
    let fields = input_fields(&surface);
    assert!(!fields[0].exclusive);
    assert_eq!(
        validate_response(&surface, &response("go", json!({"picks": ["a", "b"]}))),
        Ok(())
    );
}

#[test]
fn nested_input_paths_flatten_by_pointer_minus_slash() {
    let surface = json!({
        "version": "1.0",
        "createSurface": {"dataModel": {"a": {"b": "x"}}, "components": [
            {"id": "root", "component": "Column", "children": ["field", "go_label", "go"]},
            {"id": "field", "component": "TextField", "label": "Nested",
             "value": {"@path": "/a/b"}},
            {"id": "go_label", "component": "Text", "text": "Go"},
            {"id": "go", "component": "Button", "child": "go_label",
             "action": {"event": {"name": "go"}}}
        ]}
    });
    assert_eq!(input_fields(&surface)[0].key, "a/b");
    let data = build_data(&surface, "go", &Map::new()).unwrap();
    assert_eq!(data["a/b"], json!("x"), "default resolves nested pointers");
    assert_eq!(
        validate_response(&surface, &response("go", json!({"a/b": "y"}))),
        Ok(())
    );
}

#[test]
fn build_data_shared_fixtures_parity() {
    let json_str = include_str!("../../../tests/fixtures/a2ui-build-data-cases.json");
    let cases: Vec<Value> = serde_json::from_str(json_str).expect("parse shared fixtures");
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let surface = &case["surface"];
        let action = case["action"].as_str().unwrap();
        let values: Map<String, Value> = serde_json::from_value(case["values"].clone()).unwrap();
        let expected: Map<String, Value> =
            serde_json::from_value(case["expected"].clone()).unwrap();
        let actual = build_data(surface, action, &values).unwrap();
        assert_eq!(actual, expected, "case failed: {name}");
    }
}
