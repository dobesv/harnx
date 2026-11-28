//! Payload schemas for ordered form fields and multipart parts.

use serde_json::{json, Map, Value};

use crate::compact::{is_binary_schema, object_property_names, resolve_def_chain};

pub(super) fn text_schema(source: &Value, defs: &Map<String, Value>) -> Value {
    let resolved = resolve_def_chain(source, defs);
    if resolved["type"] == "string" && !is_binary_schema(resolved, defs) {
        // Plain text with a string source has the same shape. Object and
        // binary sources on a wildcard cannot describe the text payload.
        source.clone()
    } else {
        json!({"type": "string"})
    }
}

fn name_schema(source: &Value, defs: &Map<String, Value>) -> Value {
    match object_property_names(source, defs) {
        Some(names) => json!({"type": "string", "enum": names}),
        None => json!({"type": "string"}),
    }
}

pub(super) fn fields_schema(source: &Value, defs: &Map<String, Value>) -> Value {
    let items = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {"name": name_schema(source, defs), "value": {"type": "string"}},
        "required": ["name", "value"]
    });
    named_items(
        items,
        source,
        defs,
        "Ordered URL-encoded fields; repeat a name to send repeated values.",
    )
}

pub(super) fn parts_schema(source: &Value, defs: &Map<String, Value>) -> Value {
    let items = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "name": name_schema(source, defs),
            "text": {"type": "string"},
            "base64": {"type": "string", "description": "Base64-encoded part bytes."},
            "filename": {"type": "string"},
            "content_type": {"type": "string"}
        },
        "required": ["name"],
        "oneOf": [{"required": ["text"]}, {"required": ["base64"]}]
    });
    named_items(
        items,
        source,
        defs,
        "Ordered multipart parts; repeat a name to send repeated parts.",
    )
}

fn named_items(
    items: Value,
    source: &Value,
    defs: &Map<String, Value>,
    description: &str,
) -> Value {
    let mut array =
        json!({"type": "array", "minItems": 1, "items": items, "description": description});
    let source = resolve_def_chain(source, defs);
    let required = source["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    // Express required object fields as presence in the ordered list. Do not
    // use uniqueness: repeated names are needed for collections and uploads.
    let constraints: Vec<_> = required
        .map(|name| {
            json!({
                "contains": {"properties": {"name": {"const": name}}}
            })
        })
        .collect();
    if !constraints.is_empty() {
        array["allOf"] = json!(constraints);
    }
    array
}
