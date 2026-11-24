use super::*;

use serde_json::json;

fn block() -> Value {
    json!({
        "type": "resource",
        "resource": {
            "uri": "a2ui://surface",
            "mimeType": A2UI_MIME_TYPE,
            "text": serde_json::to_string(&surface()).unwrap(),
        },
        "annotations": { "audience": ["user"] },
    })
}

fn surface() -> Value {
    json!({
        "version": "1.0",
        "createSurface": {
            "surfaceId": "s-1",
            "components": [
                { "id": "root", "component": "Card", "child": "col" },
                { "id": "col", "component": "Column", "children": ["title", "go_label", "go"] },
                { "id": "title", "component": "Text", "text": "Hello" },
                { "id": "go_label", "component": "Text", "text": "Go" },
                { "id": "go", "component": "Button", "child": "go_label",
                  "action": { "event": { "name": "go" } } }
            ]
        }
    })
}

fn output(content: Vec<Value>) -> Value {
    json!({ "content": content, "isError": false })
}

#[test]
fn extracts_user_audience_surface_and_removes_it_from_output() {
    let mut value = output(vec![json!({"type": "text", "text": "hi"}), block()]);
    let surfaces = prepare_tool_result_surfaces(&mut value);
    assert_eq!(surfaces.len(), 1);
    assert_eq!(surfaces[0]["createSurface"]["surfaceId"], "s-1");
    assert_eq!(surfaces[0]["createSurface"]["catalogId"], BASIC_CATALOG_ID);
    assert_eq!(value["content"].as_array().unwrap().len(), 1);
    assert_eq!(value["content"][0]["type"], "text");
}

#[test]
fn non_user_audience_and_other_resources_stay_in_output() {
    let mut model_visible = block();
    model_visible["annotations"]["audience"] = json!(["assistant"]);
    let other_mime = json!({
        "type": "resource",
        "resource": { "uri": "x", "mimeType": "text/plain", "text": "hi" },
    });
    let mut value = output(vec![model_visible, other_mime]);
    let surfaces = prepare_tool_result_surfaces(&mut value);
    assert!(surfaces.is_empty());
    assert_eq!(value["content"].as_array().unwrap().len(), 2);
}

#[test]
fn output_without_content_is_untouched() {
    let mut value = json!({"done": true});
    assert!(prepare_tool_result_surfaces(&mut value).is_empty());
    assert_eq!(value, json!({"done": true}));
}

#[test]
fn invalid_envelope_fails_the_call_without_original_output() {
    let mut bad = block();
    bad["resource"]["text"] = json!("private invalid JSON");
    let mut value = output(vec![bad]);
    let surfaces = prepare_tool_result_surfaces(&mut value);
    assert!(surfaces.is_empty());
    assert_eq!(value["is_error"], true);
    assert!(value["error"].as_str().unwrap().contains("a2ui"));
    assert!(value.get("original_output").is_none());
    assert!(!value.to_string().contains("private invalid JSON"));
    assert!(!value.to_string().contains(A2UI_MIME_TYPE));
}

#[test]
fn invalid_surface_subset_fails_the_call() {
    let mut envelope = surface();
    envelope["createSurface"]["components"][4]["component"] = json!("Chart");
    let mut bad = block();
    bad["resource"]["text"] = json!(serde_json::to_string(&envelope).unwrap());
    let mut value = output(vec![bad]);
    assert!(prepare_tool_result_surfaces(&mut value).is_empty());
    assert_eq!(value["is_error"], true);
}

#[test]
fn quarantines_all_private_blocks_before_any_validation() {
    for invalid_first in [false, true] {
        let mut bad = block();
        bad["resource"]["text"] = json!("private invalid JSON");
        let mut blocks = vec![block(), bad, block()];
        if invalid_first {
            blocks.reverse();
            blocks.rotate_left(1);
        }
        blocks.insert(1, json!({"type": "text", "text": "public reply"}));
        let mut value = output(blocks);
        assert!(extract_a2ui_surfaces(&mut value).is_err());
        assert_eq!(
            value,
            output(vec![json!({"type": "text", "text": "public reply"})])
        );
    }
}

#[test]
fn invalid_private_surface_diagnostic_never_quotes_payload() {
    for field in ["version", "component", "property"] {
        let mut envelope = surface();
        match field {
            "version" => envelope["version"] = json!("private version sentinel"),
            "component" => {
                envelope["createSurface"]["components"][0]["component"] =
                    json!("private component sentinel")
            }
            _ => {
                envelope["createSurface"]["components"][0]["private property sentinel"] =
                    json!(true)
            }
        }
        let mut bad = block();
        bad["resource"]["text"] = json!(envelope.to_string());
        let mut value = output(vec![bad]);
        assert!(prepare_tool_result_surfaces(&mut value).is_empty());
        assert_eq!(
            value,
            json!({
                "is_error": true,
                "error": "invalid user-audience a2ui surface; resources omitted"
            })
        );
    }
}
