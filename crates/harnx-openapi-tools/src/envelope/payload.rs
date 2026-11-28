//! Schemas of the envelope's payload properties: `fields` and `parts`.

use serde_json::{json, Map, Value};

use crate::compact::object_property_names;

/// The `name` schema for repeated `fields`/`parts`: constrained to the source
/// object's property names when they can be derived soundly, free-form
/// otherwise.
fn name_schema(source: Option<&Value>, defs: &Map<String, Value>) -> Value {
    let names = source.and_then(|source| object_property_names(source, defs));
    match names {
        Some(names) => json!({"type": "string", "enum": names}),
        None => json!({"type": "string"}),
    }
}

pub(super) fn fields_schema(source: Option<&Value>, defs: &Map<String, Value>) -> Value {
    let mut field = Map::new();
    field.insert("name".to_string(), name_schema(source, defs));
    field.insert("value".to_string(), json!({"type": "string"}));
    json!({
        "type": "array",
        "minItems": 1,
        "description": "Ordered URL-encoded fields; repeat a name to send repeated values.",
        "items": {
            "type": "object",
            "additionalProperties": false,
            "properties": Value::Object(field),
            "required": ["name", "value"]
        }
    })
}

pub(super) fn parts_schema(source: Option<&Value>, defs: &Map<String, Value>) -> Value {
    let mut part = Map::new();
    part.insert("name".to_string(), name_schema(source, defs));
    part.insert(
        "text".to_string(),
        json!({"type": "string", "description": "Text content for this part; provide exactly one of text/base64."}),
    );
    part.insert(
        "base64".to_string(),
        json!({"type": "string", "description": "Base64 file bytes for this part; provide exactly one of text/base64."}),
    );
    part.insert("filename".to_string(), json!({"type": "string"}));
    part.insert("content_type".to_string(), json!({"type": "string"}));
    json!({
        "type": "array",
        "minItems": 1,
        "description": "Ordered multipart parts; repeat a name to send repeated parts.",
        "items": {
            "type": "object",
            "additionalProperties": false,
            "properties": Value::Object(part),
            "required": ["name"]
        }
    })
}
