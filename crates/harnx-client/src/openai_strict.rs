//! Tool schemas for OpenAI strict mode.
//!
//! Strict mode constrains decoding to the schema, but it accepts only a
//! subset of JSON Schema and requires every object to list all its
//! properties in `required` with `additionalProperties: false`. A field
//! stays optional by also accepting `null`, so each optional field that
//! does not already accept `null` is made to. The engine strips those
//! nulls from the call before the tool sees it
//! (`JsonSchema::drop_omitted_nulls`).
//!
//! A schema that cannot be expressed this way is reported as an error, and
//! the tool is sent without strict mode instead.

use harnx_core::json_schema::{accepts_null, required_names};
use harnx_core::tool::JsonSchema;
use serde_json::{json, Map, Value};
use std::collections::HashSet;

/// Keywords strict mode accepts.
const SUPPORTED_KEYWORDS: &[&str] = &[
    "type",
    "description",
    "title",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "anyOf",
    "enum",
    "const",
    "$ref",
    "$defs",
    "definitions",
    "pattern",
    "format",
    "minLength",
    "maxLength",
    "multipleOf",
    "maximum",
    "exclusiveMaximum",
    "minimum",
    "exclusiveMinimum",
    "minItems",
    "maxItems",
];

/// Annotations that carry no constraint, dropped rather than refused.
/// `default` is among them: strict mode has the model give every property
/// a value, and OpenAI does not list it as supported.
const DROPPED_ANNOTATIONS: &[&str] = &[
    "$schema",
    "default",
    "$id",
    "$comment",
    "examples",
    "deprecated",
    "readOnly",
    "writeOnly",
    "contentEncoding",
    "contentMediaType",
];

/// The only `format` values strict mode accepts, all on strings.
const STRING_FORMATS: &[&str] = &[
    "date-time",
    "time",
    "date",
    "duration",
    "email",
    "hostname",
    "ipv4",
    "ipv6",
    "uuid",
];

const MAX_OBJECT_DEPTH: usize = 10;
const MAX_PROPERTIES: usize = 5000;

/// Convert `schema` to strict mode, or say why it can't be.
pub(crate) fn strict_parameters(schema: &JsonSchema) -> Result<Value, String> {
    let mut root = schema.as_value().clone();
    if root.get("type").and_then(Value::as_str) != Some("object") {
        return Err("the root schema is not an object".into());
    }
    let mut converter = Converter { properties: 0 };
    converter.convert(&mut root, 0)?;
    Ok(root)
}

struct Converter {
    properties: usize,
}

impl Converter {
    fn convert(&mut self, node: &mut Value, depth: usize) -> Result<(), String> {
        let Value::Object(object) = node else {
            return Err(format!("boolean schema `{node}` has no strict form"));
        };
        normalize_keywords(object)?;
        if is_object_schema(object) {
            self.convert_object(object, depth + 1)?;
        }
        self.convert_items(object, depth)?;
        if let Some(Value::Array(variants)) = object.get_mut("anyOf") {
            variants
                .iter_mut()
                .try_for_each(|variant| self.convert(variant, depth))?;
        }
        for key in ["$defs", "definitions"] {
            if let Some(Value::Object(definitions)) = object.get_mut(key) {
                definitions
                    .values_mut()
                    .try_for_each(|definition| self.convert(definition, depth))?;
            }
        }
        Ok(())
    }

    fn convert_items(
        &mut self,
        object: &mut Map<String, Value>,
        depth: usize,
    ) -> Result<(), String> {
        let Some(items) = object.get_mut("items") else {
            if declares_type(object, "array") {
                return Err("an array schema has no `items`".into());
            }
            return Ok(());
        };
        if items.is_array() {
            return Err("tuple-form `items` is not supported".into());
        }
        self.convert(items, depth)
    }

    fn convert_object(
        &mut self,
        object: &mut Map<String, Value>,
        depth: usize,
    ) -> Result<(), String> {
        if depth > MAX_OBJECT_DEPTH {
            return Err(format!(
                "objects nest deeper than {MAX_OBJECT_DEPTH} levels"
            ));
        }
        match object.get("additionalProperties") {
            None | Some(Value::Bool(false)) => {}
            Some(_) => return Err("an object allows additional properties".into()),
        }
        let explicitly_closed = object.contains_key("additionalProperties");
        object.insert("additionalProperties".into(), Value::Bool(false));
        let Some(Value::Object(properties)) = object.get_mut("properties") else {
            if explicitly_closed {
                object.insert("properties".into(), json!({}));
                object.insert("required".into(), json!([]));
                return Ok(());
            }
            return Err("an object declares no properties".into());
        };
        self.properties += properties.len();
        if self.properties > MAX_PROPERTIES {
            return Err(format!("more than {MAX_PROPERTIES} properties"));
        }
        let required: HashSet<String> = required_names(object)
            .into_iter()
            .map(str::to_string)
            .collect();
        let Some(Value::Object(properties)) = object.get_mut("properties") else {
            unreachable!("checked above");
        };
        for (name, property) in properties.iter_mut() {
            let needs_null = !required.contains(name) && !accepts_null(property);
            self.convert(property, depth)?;
            if needs_null {
                make_nullable(property);
            }
        }
        let all: Vec<Value> = properties.keys().cloned().map(Value::String).collect();
        object.insert("required".into(), Value::Array(all));
        Ok(())
    }
}

/// Drop what strict mode ignores and refuse what it rejects, leaving only
/// keywords it accepts.
fn normalize_keywords(object: &mut Map<String, Value>) -> Result<(), String> {
    drop_annotations(object);
    rename_one_of(object)?;
    if let Some(keyword) = object
        .keys()
        .find(|key| !SUPPORTED_KEYWORDS.contains(&key.as_str()))
    {
        return Err(format!("strict mode does not support `{keyword}`"));
    }
    if object.contains_key("$ref") && object.len() > 1 {
        return Err("a `$ref` has sibling keywords".into());
    }
    if !constrains_value(object) {
        return Err("a schema accepts any value".into());
    }
    Ok(())
}

/// Generating a value that matches any variant is the same task as
/// matching exactly one, and the tool validates its input, so `oneOf`
/// becomes the `anyOf` strict mode accepts.
fn rename_one_of(object: &mut Map<String, Value>) -> Result<(), String> {
    let Some(variants) = object.remove("oneOf") else {
        return Ok(());
    };
    if object.insert("anyOf".into(), variants).is_some() {
        return Err("schema uses both `anyOf` and `oneOf`".into());
    }
    Ok(())
}

fn drop_annotations(object: &mut Map<String, Value>) {
    for key in DROPPED_ANNOTATIONS {
        object.remove(*key);
    }
    let keeps_format = declares_type(object, "string")
        && object
            .get("format")
            .and_then(Value::as_str)
            .is_some_and(|format| STRING_FORMATS.contains(&format));
    if !keeps_format {
        object.remove("format");
    }
}

/// Whether the schema limits its values at all. `{}` and a bare
/// `{"description": ...}` accept anything, which strict mode cannot
/// express.
fn constrains_value(object: &Map<String, Value>) -> bool {
    [
        "type",
        "properties",
        "items",
        "anyOf",
        "enum",
        "const",
        "$ref",
    ]
    .iter()
    .any(|key| object.contains_key(*key))
}

fn is_object_schema(object: &Map<String, Value>) -> bool {
    declares_type(object, "object") || object.contains_key("properties")
}

fn declares_type(object: &Map<String, Value>, name: &str) -> bool {
    match object.get("type") {
        Some(Value::String(schema_type)) => schema_type == name,
        Some(Value::Array(types)) => types.iter().any(|schema_type| schema_type == name),
        _ => false,
    }
}

/// Let an optional property also accept `null`.
fn make_nullable(property: &mut Value) {
    let Value::Object(object) = property else {
        return;
    };
    match object.get_mut("type") {
        Some(Value::String(schema_type)) => {
            let schema_type = schema_type.clone();
            object.insert("type".into(), json!([schema_type, "null"]));
        }
        Some(Value::Array(types)) => types.push(json!("null")),
        _ => {
            if let Some(Value::Array(variants)) = object.get_mut("anyOf") {
                variants.push(json!({"type": "null"}));
                return;
            }
            let inner = std::mem::take(property);
            *property = json!({"anyOf": [inner, {"type": "null"}]});
            return;
        }
    }
    if let Some(Value::Array(values)) = object.get_mut("enum") {
        values.push(Value::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strict(schema: Value) -> Result<Value, String> {
        strict_parameters(&JsonSchema::new(schema))
    }

    #[test]
    fn optional_fields_become_required_and_nullable() {
        let converted = strict(json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Plan name."},
                "body": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}, "default": []},
                "title": {"type": ["string", "null"], "default": null},
                "status": {"type": "string", "enum": ["open", "done"]}
            },
            "required": ["name"]
        }))
        .unwrap();
        assert_eq!(
            converted,
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Plan name."},
                    "body": {"type": ["string", "null"]},
                    "tags": {"type": ["array", "null"], "items": {"type": "string"}},
                    "title": {"type": ["string", "null"]},
                    "status": {"type": ["string", "null"], "enum": ["open", "done", null]}
                },
                "required": ["name", "body", "tags", "title", "status"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn nested_objects_and_any_of_variants_are_converted() {
        let converted = strict(json!({
            "type": "object",
            "properties": {
                "edit": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {
                                "old_text": {"type": "string"},
                                "replace_all": {"type": ["boolean", "null"]}
                            },
                            "required": ["old_text"],
                            "additionalProperties": false
                        },
                        {"type": "null"}
                    ]
                },
                "choice": {"oneOf": [{"const": "a"}, {"const": "b"}]}
            }
        }))
        .unwrap();
        assert_eq!(
            converted["properties"]["edit"]["anyOf"][0]["required"],
            json!(["old_text", "replace_all"])
        );
        assert_eq!(
            converted["properties"]["choice"],
            json!({"anyOf": [{"const": "a"}, {"const": "b"}, {"type": "null"}]})
        );
    }

    #[test]
    fn number_formats_are_dropped_and_string_formats_kept() {
        let converted = strict(json!({
            "type": "object",
            "properties": {
                "issue": {"type": "integer", "format": "uint64", "minimum": 0},
                "when": {"type": "string", "format": "date-time"},
                "site": {"type": "string", "format": "uri"}
            },
            "required": ["issue", "when", "site"]
        }))
        .unwrap();
        assert_eq!(
            converted["properties"]["issue"],
            json!({"type": "integer", "minimum": 0})
        );
        assert_eq!(
            converted["properties"]["when"],
            json!({"type": "string", "format": "date-time"})
        );
        assert_eq!(converted["properties"]["site"], json!({"type": "string"}));
    }

    #[test]
    fn empty_closed_object_is_accepted() {
        let converted = strict(json!({"type": "object", "additionalProperties": false})).unwrap();
        assert_eq!(
            converted,
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {},
                "required": []
            })
        );
    }

    #[test]
    fn schemas_strict_mode_cannot_express_are_refused() {
        let refused = [
            json!({"type": "string"}),
            json!({"type": "object"}),
            json!({"type": "object", "properties": {"env": {
                "type": "object", "additionalProperties": {"type": "string"}
            }}}),
            json!({"type": "object", "properties": {"value": {}}}),
            json!({"type": "object", "properties": {"list": {"type": "array"}}}),
            json!({"type": "object", "properties": {"x": {"allOf": [{"type": "string"}]}}}),
            json!({"type": "object", "properties": {"x": {"type": "array", "items": {"type": "string"}, "uniqueItems": true}}}),
            json!({"type": "object", "properties": {"x": {"$ref": "#/$defs/X", "description": "d"}}}),
        ];
        for schema in refused {
            assert!(strict(schema.clone()).is_err(), "accepted {schema}");
        }
    }

    #[test]
    fn deep_nesting_is_refused() {
        let mut schema = json!({"type": "object", "properties": {}});
        for _ in 0..MAX_OBJECT_DEPTH {
            schema =
                json!({"type": "object", "properties": {"child": schema}, "required": ["child"]});
        }
        assert!(strict(schema).is_err());
    }
}
