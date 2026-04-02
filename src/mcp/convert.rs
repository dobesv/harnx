use crate::tool::{JsonSchema, ToolDeclaration};

use anyhow::{anyhow, Result};
use indexmap::IndexMap;
use serde_json::Value;

pub fn mcp_tool_to_declaration(
    server_name: &str,
    tool_name: &str,
    tool_description: &str,
    input_schema: &Value,
) -> Result<ToolDeclaration> {
    Ok(ToolDeclaration {
        name: format!("{server_name}_{tool_name}"),
        description: tool_description.to_string(),
        parameters: convert_json_schema(input_schema)?,
        agent: false,
    })
}

fn convert_json_schema(schema: &Value) -> Result<JsonSchema> {
    let type_value = schema
        .get("type")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let description = schema
        .get("description")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let default = schema.get("default").cloned();

    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| {
            properties
                .iter()
                .map(|(name, value)| Ok((name.clone(), convert_json_schema(value)?)))
                .collect::<Result<IndexMap<_, _>>>()
        })
        .transpose()?;

    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|required| {
            required
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(ToOwned::to_owned)
                        .ok_or_else(|| anyhow!("JSON schema 'required' values must be strings"))
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?;

    let items = schema
        .get("items")
        .map(|items| convert_json_schema(items).map(Box::new))
        .transpose()?;

    let any_of = schema
        .get("anyOf")
        .and_then(Value::as_array)
        .map(|variants| {
            variants
                .iter()
                .map(convert_json_schema)
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?;

    let enum_value = schema
        .get("enum")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .map(|value| match value {
                    Value::String(value) => Ok(value.clone()),
                    other => Ok(other.to_string()),
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?;

    Ok(JsonSchema {
        type_value,
        description,
        properties,
        items,
        any_of,
        enum_value,
        default,
        required,
    })
}

#[cfg(test)]
mod tests {
    use super::{convert_json_schema, mcp_tool_to_declaration};
    use serde_json::json;

    #[test]
    fn mcp_convert_json_schema_handles_nested_properties() {
        let schema = json!({
            "type": "object",
            "description": "Tool input",
            "required": ["path"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path"
                },
                "options": {
                    "type": "array",
                    "items": {
                        "type": "string"
                    }
                },
                "mode": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ],
                    "default": null
                },
                "format": {
                    "type": "string",
                    "enum": ["json", "text"]
                }
            }
        });

        let converted = convert_json_schema(&schema).expect("convert schema");

        assert_eq!(converted.type_value.as_deref(), Some("object"));
        assert_eq!(converted.description.as_deref(), Some("Tool input"));
        assert_eq!(
            converted.required.as_deref(),
            Some(&["path".to_string()][..])
        );

        let properties = converted.properties.expect("properties");
        assert_eq!(
            properties
                .get("path")
                .and_then(|schema| schema.type_value.as_deref()),
            Some("string")
        );
        assert_eq!(
            properties
                .get("options")
                .and_then(|schema| schema.items.as_ref())
                .and_then(|schema| schema.type_value.as_deref()),
            Some("string")
        );
        assert_eq!(
            properties
                .get("mode")
                .and_then(|schema| schema.any_of.as_ref())
                .map(Vec::len),
            Some(2)
        );
        assert_eq!(
            properties
                .get("format")
                .and_then(|schema| schema.enum_value.as_deref()),
            Some(&["json".to_string(), "text".to_string()][..])
        );
    }

    #[test]
    fn mcp_tool_to_function_prefixes_tool_name_and_disables_agent_mode() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            }
        });

        let function =
            mcp_tool_to_declaration("filesystem", "read_file", "Read a file from disk", &schema)
                .expect("convert tool");

        assert_eq!(function.name, "filesystem_read_file");
        assert_eq!(function.description, "Read a file from disk");
        assert!(!function.agent);
        assert_eq!(function.parameters.type_value.as_deref(), Some("object"));
    }
}
