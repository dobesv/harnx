//! Elicitation `requestedSchema` ↔ A2UI surface derivation, the documented
//! T4.1 subset shared by both directions of the challenge mapping.

use std::collections::BTreeMap;

use rmcp::model::{ElicitationSchema, PrimitiveSchemaDefinition};
use serde_json::{json, Map, Value};

pub(super) fn empty_requested_schema() -> ElicitationSchema {
    serde_json::from_value(json!({"type": "object", "properties": {}}))
        .expect("static empty elicitation schema is valid")
}

/// Derive an elicitation `requestedSchema` from an a2ui surface's input
/// components, so third-party MCP clients can still render a usable form.
/// The full surface rides in the result's `_meta` for harnx clients.
pub(super) fn requested_schema_from_surface(
    surface: Option<&Value>,
) -> Result<ElicitationSchema, String> {
    let surface = surface
        .ok_or_else(|| "a2ui challenge has no surface to derive a schema from".to_string())?;
    let mut properties = Map::new();
    for field in harnx_core::interaction::response::input_fields(surface) {
        let property = match field.kind {
            harnx_core::interaction::response::InputKind::TextField => json!({"type": "string"}),
            harnx_core::interaction::response::InputKind::ChoicePicker => json!({
                "type": "string",
                "enum": field
                    .options
                    .iter()
                    .map(|option| option.value.clone())
                    .collect::<Vec<_>>(),
            }),
        };
        properties.insert(field.key.clone(), property);
    }
    let schema = json!({"type": "object", "properties": properties});
    serde_json::from_value(schema)
        .map_err(|error| format!("derived elicitation schema is invalid: {error}"))
}

/// Build an a2ui surface (T4.1 subset) from elicitation schema properties.
///
/// Documented subset: string properties become text fields, enum properties
/// become choice pickers; numbers, integers and booleans are unsupported and
/// error out. The single submit button's action is `submit`, and every
/// property's answer lands in `data` under the property name.
pub(super) fn surface_from_requested_schema(
    message: &str,
    properties: &BTreeMap<String, PrimitiveSchemaDefinition>,
) -> Result<Value, String> {
    let mut data_model = Map::new();
    let mut fields = Vec::new();
    let mut children = vec!["prompt".to_string()];
    for (key, definition) in properties {
        let (model, component) = field_from_property(key, definition)?;
        data_model.insert(key.clone(), model);
        fields.push(component);
        children.push(format!("field-{key}"));
    }
    children.extend(["submit_label", "submit"].map(str::to_string));
    let mut components = vec![
        json!({"id": "root", "component": "Card", "child": "col"}),
        json!({"id": "col", "component": "Column", "children": children}),
        json!({"id": "prompt", "component": "Text", "text": message}),
    ];
    components.extend(fields);
    components.push(json!({
        "id": "submit_label",
        "component": "Text",
        "text": "Submit",
    }));
    components.push(json!({
        "id": "submit",
        "component": "Button",
        "child": "submit_label",
        "action": {"event": {"name": "submit"}},
    }));
    Ok(json!({
        "version": "1.0",
        "createSurface": {
            "dataModel": data_model,
            "components": components,
        }
    }))
}

/// Map one elicitation property onto its data-model entry and its A2UI
/// component (id, kind, value binding).
fn field_from_property(
    key: &str,
    definition: &PrimitiveSchemaDefinition,
) -> Result<(Value, Value), String> {
    let property = serde_json::to_value(definition)
        .map_err(|error| format!("elicitation property '{key}' is not representable: {error}"))?;
    let id = format!("field-{key}");
    if let Some(enum_values) = property.get("enum").and_then(Value::as_array) {
        let options: Vec<Value> = enum_values
            .iter()
            .map(|value| json!({"label": value, "value": value}))
            .collect();
        if options.is_empty() {
            return Err(format!(
                "elicitation property '{key}' has an empty option list"
            ));
        }
        return Ok((
            json!([]),
            json!({
                "id": id,
                "component": "ChoicePicker",
                "options": options,
                "value": {"@path": format!("/{key}")},
            }),
        ));
    }
    if property.get("type") == Some(&json!("string")) {
        let label = property
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or(key);
        return Ok((
            json!(""),
            json!({
                "id": id,
                "component": "TextField",
                "label": label,
                "value": {"@path": format!("/{key}")},
            }),
        ));
    }
    let kind = property.get("type").and_then(Value::as_str).unwrap_or("?");
    Err(format!(
        "elicitation property '{key}' uses type '{kind}', which harnx cannot render"
    ))
}
