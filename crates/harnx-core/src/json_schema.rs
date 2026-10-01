//! Tool parameter schemas, kept as the tool declared them.
//!
//! Providers read different dialects of JSON Schema: OpenAI strict mode
//! wants every property required, and some providers resolve no `$ref`.
//! Each client converts at its own request boundary, so the canonical copy
//! must keep everything a converter could need, including `null` in a
//! `type` list, `additionalProperties` and number bounds.

use schemars::transform::{transform_subschemas, Transform};
use schemars::Schema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JsonSchema(Value);

impl Default for JsonSchema {
    fn default() -> Self {
        Self(Value::Object(Map::new()))
    }
}

impl JsonSchema {
    pub fn new(value: Value) -> Self {
        Self(value)
    }

    /// Accept a schema a tool server registered. Local `$ref`s are inlined,
    /// because several providers resolve no references at all. A reference
    /// that recurses stays a `$ref`, with the definitions it needs.
    pub fn from_tool_schema(mut value: Value) -> Result<Self, String> {
        let Some(object) = value.as_object_mut() else {
            return Err(format!("schema must be a JSON object, got {value}"));
        };
        object.remove("$schema");
        inline_local_refs(&mut value);
        Ok(Self(value))
    }

    pub fn as_value(&self) -> &Value {
        &self.0
    }

    pub fn into_value(self) -> Value {
        self.0
    }

    pub fn properties(&self) -> Option<&Map<String, Value>> {
        self.0.get("properties").and_then(Value::as_object)
    }

    pub fn is_empty_properties(&self) -> bool {
        self.properties().is_none_or(Map::is_empty)
    }

    /// Remove `null` arguments the model sent for optional properties
    /// whose schema does not accept `null`, so the tool sees them as
    /// omitted. OpenAI strict mode makes the model send such nulls, since
    /// it has to give every property a value, and other models send them
    /// unprompted.
    pub fn drop_omitted_nulls(&self, arguments: &mut Value) {
        drop_omitted_nulls(&self.0, arguments);
    }
}

/// Keywords that limit a schema's values. A schema with none of them,
/// such as `{}`, accepts anything.
const CONSTRAINING_KEYWORDS: &[&str] = &["$ref", "properties", "items", "allOf"];

/// Whether a value of `null` satisfies `schema`. A schema that
/// constrains nothing, such as `{}`, accepts it. An unresolved `$ref` is
/// treated as not accepting it.
pub fn accepts_null(schema: &Value) -> bool {
    match schema {
        Value::Bool(accepts) => *accepts,
        Value::Object(object) => object_accepts_null(object),
        _ => false,
    }
}

fn object_accepts_null(object: &Map<String, Value>) -> bool {
    if object.get("nullable") == Some(&Value::Bool(true)) {
        return true;
    }
    if let Some(schema_type) = object.get("type") {
        return type_includes_null(schema_type);
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        return values.contains(&Value::Null);
    }
    if let Some(value) = object.get("const") {
        return value.is_null();
    }
    if let Some(variants) = union_variants(object) {
        return variants.iter().any(accepts_null);
    }
    !CONSTRAINING_KEYWORDS
        .iter()
        .any(|key| object.contains_key(*key))
}

fn type_includes_null(schema_type: &Value) -> bool {
    match schema_type {
        Value::String(name) => name == "null",
        Value::Array(names) => names.iter().any(|name| name == "null"),
        _ => false,
    }
}

/// The variants of an `anyOf` or `oneOf` union.
fn union_variants(object: &Map<String, Value>) -> Option<&Vec<Value>> {
    ["anyOf", "oneOf"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_array))
}

/// The names in a schema's `required` list.
pub fn required_names(schema: &Map<String, Value>) -> HashSet<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn inline_local_refs(root: &mut Value) {
    let Some(object) = root.as_object_mut() else {
        return;
    };
    let removed: Vec<(&str, Value)> = ["$defs", "definitions"]
        .into_iter()
        .filter_map(|key| object.remove(key).map(|value| (key, value)))
        .collect();
    let definitions: Map<String, Value> = removed
        .iter()
        .flat_map(|(key, value)| {
            value
                .as_object()
                .into_iter()
                .flatten()
                .map(move |(name, schema)| (format!("#/{key}/{name}"), schema.clone()))
        })
        .collect();
    let mut inliner = RefInliner {
        definitions: &definitions,
        expanding: Vec::new(),
        kept_recursive_ref: false,
    };
    if let Ok(schema) = <&mut Schema>::try_from(&mut *root) {
        inliner.transform(schema);
    }
    // A `$ref` that recurses still points into the definitions, so they go
    // back unchanged.
    if let (true, Some(object)) = (inliner.kept_recursive_ref, root.as_object_mut()) {
        object.extend(
            removed
                .into_iter()
                .map(|(key, value)| (key.to_string(), value)),
        );
    }
}

struct RefInliner<'a> {
    definitions: &'a Map<String, Value>,
    expanding: Vec<String>,
    kept_recursive_ref: bool,
}

impl Transform for RefInliner<'_> {
    fn transform(&mut self, schema: &mut Schema) {
        let reference = schema
            .get("$ref")
            .and_then(Value::as_str)
            .filter(|reference| self.definitions.contains_key(*reference))
            .map(str::to_string);
        let Some(reference) = reference else {
            transform_subschemas(self, schema);
            return;
        };
        if self.expanding.contains(&reference) {
            self.kept_recursive_ref = true;
            return;
        }
        let Ok(mut expanded) = Schema::try_from(self.definitions[&reference].clone()) else {
            return;
        };
        if let (Some(target), Some(siblings)) = (expanded.as_object_mut(), schema.as_object_mut()) {
            // Keywords beside a `$ref`, such as a field's own description,
            // describe this use of the definition and win over it.
            siblings.remove("$ref");
            target.extend(std::mem::take(siblings));
        }
        *schema = expanded;
        self.expanding.push(reference);
        self.transform(schema);
        self.expanding.pop();
    }
}

fn drop_omitted_nulls(schema: &Value, value: &mut Value) {
    let Some(schema) = structure_schema_for(schema, value) else {
        return;
    };
    match value {
        Value::Object(arguments) => drop_omitted_object_nulls(schema, arguments),
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                items
                    .iter_mut()
                    .for_each(|item| drop_omitted_nulls(item_schema, item));
            }
        }
        _ => {}
    }
}

fn drop_omitted_object_nulls(schema: &Map<String, Value>, arguments: &mut Map<String, Value>) {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    let required = required_names(schema);
    arguments.retain(|name, argument| {
        let omitted = argument.is_null()
            && !required.contains(name.as_str())
            && properties
                .get(name)
                .is_some_and(|property| !accepts_null(property));
        !omitted
    });
    arguments
        .iter_mut()
        .filter_map(|(name, argument)| Some((properties.get(name)?, argument)))
        .for_each(|(property, argument)| drop_omitted_nulls(property, argument));
}

/// The schema that describes `value`'s structure: `schema` itself, or the
/// first `anyOf`/`oneOf` variant that describes an object or array, as an
/// optional object field's `anyOf: [{...}, {"type": "null"}]` does.
fn structure_schema_for<'a>(schema: &'a Value, value: &Value) -> Option<&'a Map<String, Value>> {
    let object = schema.as_object()?;
    let structure_key = match value {
        Value::Object(_) => "properties",
        Value::Array(_) => "items",
        _ => return None,
    };
    if object.contains_key(structure_key) {
        return Some(object);
    }
    ["anyOf", "oneOf"]
        .iter()
        .filter_map(|key| object.get(*key).and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_object)
        .find(|variant| variant.contains_key(structure_key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn from_tool_schema_inlines_local_refs_and_drops_definitions() {
        let schema = JsonSchema::from_tool_schema(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "edit": {
                    "description": "The edit to apply.",
                    "anyOf": [{"$ref": "#/$defs/Edit"}, {"type": "null"}]
                },
                "tasks": {"type": "array", "items": {"$ref": "#/definitions/Task"}}
            },
            "$defs": {
                "Edit": {
                    "type": "object",
                    "properties": {"old_text": {"type": "string"}},
                    "required": ["old_text"]
                }
            },
            "definitions": {
                "Task": {"type": "object", "properties": {"title": {"type": "string"}}}
            }
        }))
        .unwrap();
        assert_eq!(
            schema.as_value(),
            &json!({
                "type": "object",
                "properties": {
                    "edit": {
                        "description": "The edit to apply.",
                        "anyOf": [
                            {
                                "type": "object",
                                "properties": {"old_text": {"type": "string"}},
                                "required": ["old_text"]
                            },
                            {"type": "null"}
                        ]
                    },
                    "tasks": {
                        "type": "array",
                        "items": {"type": "object", "properties": {"title": {"type": "string"}}}
                    }
                }
            })
        );
    }

    #[test]
    fn from_tool_schema_keeps_recursive_refs() {
        let schema = JsonSchema::from_tool_schema(json!({
            "type": "object",
            "properties": {"root": {"$ref": "#/$defs/Node"}},
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "children": {"type": "array", "items": {"$ref": "#/$defs/Node"}}
                    }
                }
            }
        }))
        .unwrap();
        let value = schema.as_value();
        assert_eq!(
            value["properties"]["root"]["properties"]["children"]["items"],
            json!({"$ref": "#/$defs/Node"})
        );
        assert!(value.get("$defs").is_some());
    }

    #[test]
    fn from_tool_schema_rejects_non_objects() {
        assert!(JsonSchema::from_tool_schema(json!(true)).is_err());
    }

    #[test]
    fn ref_siblings_override_the_definition() {
        let schema = JsonSchema::from_tool_schema(json!({
            "type": "object",
            "properties": {"mode": {"$ref": "#/$defs/Mode", "description": "Field text."}},
            "$defs": {"Mode": {"type": "string", "description": "Type text."}}
        }))
        .unwrap();
        assert_eq!(
            schema.as_value()["properties"]["mode"],
            json!({"type": "string", "description": "Field text."})
        );
    }

    #[test]
    fn accepts_null_reads_every_nullable_form() {
        assert!(accepts_null(&json!({"type": ["string", "null"]})));
        assert!(accepts_null(
            &json!({"anyOf": [{"type": "string"}, {"type": "null"}]})
        ));
        assert!(accepts_null(&json!({"type": "string", "nullable": true})));
        assert!(accepts_null(&json!({"enum": ["a", null]})));
        assert!(accepts_null(&json!({"description": "anything"})));
        assert!(!accepts_null(&json!({"type": "string"})));
        assert!(!accepts_null(&json!({"type": "array", "items": {}})));
        assert!(!accepts_null(&json!({"$ref": "#/$defs/Node"})));
    }

    #[test]
    fn drop_omitted_nulls_removes_only_optional_non_nullable_nulls() {
        let schema = JsonSchema::new(json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "parent": {"type": ["integer", "null"]},
                "edit": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {
                                "old_text": {"type": "string"},
                                "replace_all": {"type": "boolean"}
                            },
                            "required": ["old_text"]
                        },
                        {"type": "null"}
                    ]
                },
                "tasks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"title": {"type": "string"}, "body": {"type": "string"}},
                        "required": ["title"]
                    }
                }
            },
            "required": ["name"]
        }));
        let mut arguments = json!({
            "name": null,
            "tags": null,
            "parent": null,
            "edit": {"old_text": "a", "replace_all": null},
            "tasks": [{"title": "t", "body": null}],
            "unknown": null
        });
        schema.drop_omitted_nulls(&mut arguments);
        assert_eq!(
            arguments,
            json!({
                "name": null,
                "parent": null,
                "edit": {"old_text": "a"},
                "tasks": [{"title": "t"}],
                "unknown": null
            })
        );
    }
}
