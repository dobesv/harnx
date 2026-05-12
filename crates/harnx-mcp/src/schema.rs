use rmcp::schemars::Schema;
use serde_json::{Map, Value};

/// Build a JSON Schema object where each property carries a `description`.
/// Signature matches `harnx-mcp-plans`'s gold-standard helper.
pub fn object_schema_with_desc(properties: Vec<(&str, &str, Schema)>, required: &[&str]) -> Schema {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));

    let mut property_map = Map::new();
    for (name, desc, property_schema) in properties {
        let mut prop = property_schema.as_value().clone();
        if let Some(obj) = prop.as_object_mut() {
            obj.insert("description".to_string(), Value::String(desc.to_string()));
        }
        property_map.insert(name.to_string(), prop);
    }
    schema.insert("properties".to_string(), Value::Object(property_map));
    schema.insert("additionalProperties".to_string(), Value::Bool(false));

    if !required.is_empty() {
        schema.insert(
            "required".to_string(),
            Value::Array(
                required
                    .iter()
                    .map(|n| Value::String((*n).to_string()))
                    .collect(),
            ),
        );
    }
    schema.into()
}
