//! Unit tests for the interact surface validator. Every case is an edit of
//! the same valid form, applied by `rejection`, so each test reads as the
//! mutation it makes.

mod graph;

use super::*;
use serde_json::{json, Map, Value};

fn valid_form() -> Value {
    json!({
        "version": "1.0",
        "createSurface": {
            "surfaceId": "script-supplied",
            "catalogId": "https://example.com/catalog",
            "dataModel": {"choice": ["a"], "name": ""},
            "components": [
                {"id": "root", "component": "Card", "child": "col"},
                {"id": "col", "component": "Column",
                 "children": ["title", "choice", "name", "submit_label", "submit"],
                 "justify": "start", "align": "stretch"},
                {"id": "title", "component": "Text", "text": "Pick one",
                 "variant": "caption"},
                {"id": "choice", "component": "ChoicePicker", "label": "Choice",
                 "variant": "mutuallyExclusive",
                 "options": [
                     {"label": "First", "value": "a"},
                     {"label": "Second", "value": "b"}
                 ],
                 "value": {"@path": "/choice"}},
                {"id": "name", "component": "TextField", "label": "Name",
                 "value": {"@path": "/name"}, "placeholder": "Your name",
                 "variant": "shortText"},
                {"id": "submit_label", "component": "Text", "text": "Submit"},
                {"id": "submit", "component": "Button", "child": "submit_label",
                 "variant": "primary",
                 "action": {"event": {"name": "submit", "userMessage": "Submitted",
                     "context": {"choice": {"@path": "/choice"},
                                 "name": {"@path": "/name"},
                                 "literal": 3}}}}
            ],
            "metadata": {"extensions": {"harnx": {"visibility": "note"}}}
        }
    })
}

/// The validation error of the valid form with `edit` applied.
fn rejection(edit: impl FnOnce(&mut Value)) -> String {
    let mut form = valid_form();
    edit(&mut form);
    validate_surface(&form).unwrap_err()
}

/// The valid form with `edit` applied, which must still validate.
fn acceptance(edit: impl FnOnce(&mut Value)) {
    let mut form = valid_form();
    edit(&mut form);
    validate_surface(&form).unwrap();
}

/// The edited form must fail validation, naming every `expected` fragment.
fn rejects(edit: impl FnOnce(&mut Value), expected: &[&str]) {
    let error = rejection(edit);
    for fragment in expected {
        assert!(error.contains(fragment), "error was: {error}");
    }
}

fn create(form: &mut Value) -> &mut Map<String, Value> {
    form["createSurface"].as_object_mut().unwrap()
}

#[test]
fn interaction_surface_accepts_valid_form() {
    acceptance(|_| {});
}

#[test]
fn interaction_surface_accepts_surface_without_optional_fields() {
    acceptance(|form| {
        let create = create(form);
        create.remove("surfaceId");
        create.remove("catalogId");
        create.remove("dataModel");
        create.remove("metadata");
    });
}

#[test]
fn interaction_surface_rejects_wrong_version() {
    rejects(
        |form| form["version"] = json!("0.9"),
        &["version must be \"1.0\""],
    );
    rejects(
        |form| {
            form.as_object_mut().unwrap().remove("version");
        },
        &["version"],
    );
}

#[test]
fn interaction_surface_rejects_non_create_surface_messages() {
    rejects(
        |form| {
            form.as_object_mut().unwrap().remove("createSurface");
            form["updateComponents"] = json!({"surfaceId": "x", "components": []});
        },
        &["single createSurface"],
    );
}

#[test]
fn interaction_surface_rejects_unknown_component() {
    rejects(
        |form| form["createSurface"]["components"][3]["component"] = json!("Row"),
        &["'Row'", "Basic Catalog subset"],
    );
}

#[test]
fn interaction_surface_rejects_missing_textfield_value() {
    rejects(
        |form| {
            form["createSurface"]["components"][4]
                .as_object_mut()
                .unwrap()
                .remove("value");
        },
        &["name", "missing required field 'value'"],
    );
}

#[test]
fn interaction_surface_rejects_unbound_input_value() {
    rejects(
        |form| form["createSurface"]["components"][3]["value"] = json!(["a"]),
        &["@path"],
    );
}

#[test]
fn interaction_surface_rejects_function_calls() {
    rejects(
        |form| {
            form["createSurface"]["components"][4]["value"] = json!({"@call": "formatDate"});
        },
        &["function calls"],
    );
    // check conditions may not call functions either
    rejects(
        |form| {
            form["createSurface"]["components"][4]["@checks"] =
                json!([{"condition": {"@call": "required"}}]);
        },
        &["function calls"],
    );
}

#[test]
fn interaction_surface_rejects_extra_keys_in_bindings() {
    rejects(
        |form| {
            form["createSurface"]["components"][4]["value"] =
                json!({"@path": "/name", "fallback": "anonymous"});
        },
        &["unexpected key 'fallback'"],
    );
}

#[test]
fn interaction_surface_rejects_function_calls_nested_in_arrays() {
    rejects(
        |form| {
            form["createSurface"]["components"][6]["action"]["event"]["context"] =
                json!({"tags": [{"@call": "upper"}]});
        },
        &["function calls"],
    );
}

#[test]
fn interaction_surface_rejects_button_function_call_action() {
    rejects(
        |form| {
            form["createSurface"]["components"][6]["action"] =
                json!({"functionCall": {"@call": "openUrl"}});
        },
        &["action.event"],
    );
}

#[test]
fn interaction_surface_rejects_missing_root_and_dangling_refs() {
    rejects(
        |form| form["createSurface"]["components"][0]["id"] = json!("not-root"),
        &["'root'"],
    );
    rejects(
        |form| {
            form["createSurface"]["components"][1]["children"][0] = json!("missing");
        },
        &["missing"],
    );
    rejects(
        |form| form["createSurface"]["components"][6]["child"] = json!("missing"),
        &["missing"],
    );
}

#[test]
fn interaction_surface_rejects_duplicate_ids() {
    rejects(
        |form| form["createSurface"]["components"][5]["id"] = json!("title"),
        &["duplicate component id"],
    );
}

#[test]
fn interaction_surface_rejects_surface_without_button_action() {
    rejects(
        |form| {
            form["createSurface"]["components"][6]["action"]["event"]["name"] = json!("");
        },
        &["non-empty string"],
    );
    rejects(
        |form| {
            form["createSurface"]["components"][6]
                .as_object_mut()
                .unwrap()
                .remove("action");
        },
        &["missing required field 'action'"],
    );
    // A surface with no Button at all can never be answered.
    rejects(
        |form| {
            form["createSurface"]["components"][1]["children"][4] = json!("submit_label");
            form["createSurface"]["components"]
                .as_array_mut()
                .unwrap()
                .pop();
        },
        &["Button"],
    );
}

#[test]
fn interaction_surface_rejects_unknown_and_invalid_properties() {
    rejects(
        |form| form["createSurface"]["components"][2]["unknown"] = json!(true),
        &["unknown property 'unknown'"],
    );
    rejects(
        |form| form["createSurface"]["components"][2]["variant"] = json!("heading"),
        &["heading"],
    );
    rejects(
        |form| form["createSurface"]["unknown"] = json!(true),
        &["unknown field"],
    );
    rejects(
        |form| form["createSurface"]["metadata"]["unknown"] = json!(true),
        &["unknown field"],
    );
}

#[test]
fn interaction_surface_rejects_missing_components() {
    rejects(
        |form| {
            create(form).remove("components");
        },
        &["components is required"],
    );
}

#[test]
fn interaction_surface_rejects_choice_option_without_label_or_value() {
    rejects(
        |form| {
            form["createSurface"]["components"][3]["options"][0]
                .as_object_mut()
                .unwrap()
                .remove("label");
        },
        &["missing required field 'label'"],
    );
}

#[test]
fn interaction_surface_accepts_column_template_children() {
    acceptance(|form| {
        form["createSurface"]["components"][1]["children"] =
            json!({"componentId": "title", "path": "/items"});
    });
    rejects(
        |form| {
            form["createSurface"]["components"][1]["children"] =
                json!({"componentId": "missing", "path": "/items"});
        },
        &["missing"],
    );
}
