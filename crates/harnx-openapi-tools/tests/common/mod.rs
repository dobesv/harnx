//! Shared test helpers: fixture loading and request-argument synthesis.

use std::path::Path;

use serde_json::{Map, Value};

use harnx_openapi_tools::SpecDocument;

pub fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

pub fn load_fixture(name: &str) -> SpecDocument {
    SpecDocument::from_path(&fixture_path(name))
        .unwrap_or_else(|error| panic!("failed to load fixture {name}: {error}"))
}

/// A minimal value that satisfies `schema`, following only `required` paths
/// and resolving `#/$defs/…` references through the schema's own definitions.
///
/// For body envelopes this also fills the payload property the first allowed
/// `encoding` selects, so a synthesized request exercises the operation's
/// concrete encoder shape. This is a test-side proof that every operation has
/// at least one constructible request through its published input schema;
/// actual wire dispatch belongs to T3.
pub fn synthesize(schema: &Value, defs: &Map<String, Value>) -> Value {
    // A schema may itself be a bare `#/$defs/…` reference (the envelope's
    // `value` property, for example); resolve it before synthesizing.
    let schema = resolve(schema, defs);
    match schema {
        Value::Bool(true) => Value::Null,
        Value::Bool(false) => Value::Null,
        Value::Object(object) => synthesize_object(object, defs),
        _ => Value::Null,
    }
}

fn resolve<'a>(schema: &'a Value, defs: &'a Map<String, Value>) -> &'a Value {
    let mut current = schema;
    for _ in 0..16 {
        let Some(reference) = current.get("$ref").and_then(Value::as_str) else {
            break;
        };
        let Some(name) = reference.strip_prefix("#/$defs/") else {
            break;
        };
        match defs.get(name) {
            Some(next) => current = next,
            None => break,
        }
    }
    current
}

fn synthesize_object(object: &Map<String, Value>, defs: &Map<String, Value>) -> Value {
    // `allOf` composition: merge the members' constraints into one object.
    if let Some(members) = object.get("allOf").and_then(Value::as_array) {
        let merged = merge_members(members, defs);
        return synthesize_object(&merged, defs);
    }
    if let Some(variants) = ["anyOf", "oneOf"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_array))
    {
        return synthesize_variant(variants, defs);
    }
    if let Some(value) = object.get("const") {
        return value.clone();
    }
    if let Some(first) = object
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|v| v.first())
    {
        return first.clone();
    }
    synthesize_typed(object, defs)
}

/// Merge `allOf` member schemas into one object shape.
fn merge_members(members: &[Value], defs: &Map<String, Value>) -> Map<String, Value> {
    let mut merged = Map::new();
    for member in members {
        let member = resolve(member, defs);
        let Value::Object(member) = member else {
            continue;
        };
        merge_schema_object(member, &mut merged);
    }
    merged
}

/// Synthesize the first variant that constrains something.
fn synthesize_variant(variants: &[Value], defs: &Map<String, Value>) -> Value {
    for variant in variants {
        let variant = resolve(variant, defs);
        // A `{}` or boolean variant accepts null; keep looking for a
        // shape only until one constrains something.
        if variant == &Value::Object(Map::new()) {
            continue;
        }
        return synthesize(variant, defs);
    }
    Value::Null
}

/// Synthesize a value for the schema's declared `type`.
fn synthesize_typed(object: &Map<String, Value>, defs: &Map<String, Value>) -> Value {
    let schema_type = object.get("type").and_then(Value::as_str);
    match schema_type {
        Some("string") => synthesize_string(object),
        Some("integer") | Some("number") => synthesize_number(object, schema_type),
        Some("boolean") => Value::Bool(true),
        Some("array") => synthesize_array(object, defs),
        Some("object") | None => synthesize_properties(object, defs),
        Some(_) => Value::Null,
    }
}

fn synthesize_string(object: &Map<String, Value>) -> Value {
    // A patterned string is the body envelope's `content_type`; any
    // concrete `type/subtype` value satisfies the published pattern.
    if object.get("pattern").is_some() {
        let base = "application/octet-stream".to_string();
        if let Some(min_length) = object.get("minLength").and_then(Value::as_u64) {
            let min_length = min_length as usize;
            if base.len() < min_length {
                return Value::String(format!("{base}{}", "x".repeat(min_length - base.len())));
            }
        }
        return Value::String(base);
    }
    if let Some(min_length) = object.get("minLength").and_then(Value::as_u64) {
        return Value::String("x".repeat(min_length as usize));
    }
    Value::String(String::new())
}

fn synthesize_number(object: &Map<String, Value>, schema_type: Option<&str>) -> Value {
    let minimum = object.get("minimum").and_then(Value::as_f64).unwrap_or(0.0);
    let exclusive = object.get("exclusiveMinimum") == Some(&Value::Bool(true));
    let value = if exclusive { minimum + 1.0 } else { minimum };
    match schema_type {
        Some("integer") => Value::from(value as i64),
        _ => serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::from(0)),
    }
}

fn synthesize_array(object: &Map<String, Value>, defs: &Map<String, Value>) -> Value {
    let min_items = object.get("minItems").and_then(Value::as_u64).unwrap_or(1) as usize;
    let items = resolve(object.get("items").unwrap_or(&Value::Null), defs);
    Value::Array(vec![synthesize(items, defs); min_items])
}

fn merge_schema_object(source: &Map<String, Value>, merged: &mut Map<String, Value>) {
    for (key, value) in source {
        match (merged.get_mut(key), value) {
            (Some(Value::Object(existing)), Value::Object(incoming)) => {
                for (inner_key, inner) in incoming {
                    if !existing.contains_key(inner_key) {
                        existing.insert(inner_key.clone(), inner.clone());
                    }
                }
            }
            _ => {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
}

fn synthesize_properties(object: &Map<String, Value>, defs: &Map<String, Value>) -> Value {
    let properties = object.get("properties").and_then(Value::as_object);
    let required: Vec<String> = object
        .get("required")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let mut value = Map::new();
    for name in &required {
        let property = properties
            .and_then(|properties| properties.get(name))
            .unwrap_or(&Value::Null);
        let property = resolve(property, defs);
        let mut synthesized = synthesize(property, defs);
        // A required nullable property still needs a present, non-null value
        // when the type does not accept null.
        if synthesized.is_null()
            && !property
                .get("nullable")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            synthesized = Value::String(String::new());
        }
        value.insert(name.clone(), synthesized);
    }
    // Body envelope: fill the payload property matching the first `encoding`.
    if let Some(properties) = properties.filter(|properties| properties.contains_key("encoding")) {
        let encoding = value
            .get("encoding")
            .and_then(Value::as_str)
            .unwrap_or("json");
        let payload = match encoding {
            "text" => Some(Value::String(String::new())),
            "form" => properties
                .get("fields")
                .and_then(|fields| fields.get("items"))
                .map(|_| Value::Array(vec![field_pair(properties, defs)])),
            "multipart" => properties
                .get("parts")
                .and_then(|parts| parts.get("items"))
                .map(|_| {
                    let name = first_enum_value(properties, "parts", "name")
                        .unwrap_or_else(|| "part".to_string());
                    Value::Array(vec![serde_json::json!({"name": name, "base64": ""})])
                }),
            "base64" => Some(Value::String(String::new())),
            _ => properties
                .get("value")
                .map(|payload| synthesize(payload, defs)),
        };
        if let Some(payload) = payload {
            let key = match encoding {
                "text" => "text",
                "form" => "fields",
                "multipart" => "parts",
                "base64" => "base64",
                _ => "value",
            };
            value.insert(key.to_string(), payload);
        }
    }
    Value::Object(value)
}

fn field_pair(properties: &Map<String, Value>, defs: &Map<String, Value>) -> Value {
    let name =
        first_enum_value(properties, "fields", "name").unwrap_or_else(|| "field".to_string());
    let _ = defs;
    serde_json::json!({"name": name, "value": ""})
}

fn first_enum_value(
    properties: &Map<String, Value>,
    array_prop: &str,
    item_prop: &str,
) -> Option<String> {
    properties
        .get(array_prop)?
        .get("items")?
        .get("properties")?
        .get(item_prop)?
        .get("enum")?
        .as_array()?
        .first()?
        .as_str()
        .map(str::to_string)
}
