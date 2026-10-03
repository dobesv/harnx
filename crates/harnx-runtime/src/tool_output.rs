//! Lossless tool inspection and result formatting for CLI and TUI frontends.
//!
//! These helpers don't select or invoke tools. Callers must pass the declarations
//! available to their context and the complete result, not extracted text parts.

use harnx_core::tool::ToolDeclaration;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolOutputFormat {
    #[default]
    Human,
    Json,
}

impl ToolOutputFormat {
    pub fn from_json_flag(json: bool) -> Self {
        if json {
            Self::Json
        } else {
            Self::Human
        }
    }
}

/// Include runtime metadata that `ToolDeclaration` deliberately skips in its
/// persistence/LLM serialization. Keep the canonical declaration unchanged.
pub fn tool_declaration_value(tool: &ToolDeclaration) -> Value {
    // Exhaustive destructuring makes new declaration fields a compile error here.
    let ToolDeclaration {
        name,
        description,
        parameters,
        mcp_tool_name,
        mcp_server_name,
        call_template,
        result_template,
        idempotent_hint,
        read_only_hint,
        kind,
    } = tool;
    json!({
        "name": name,
        "description": description,
        "parameters": parameters,
        "mcp_tool_name": mcp_tool_name,
        "mcp_server_name": mcp_server_name,
        "call_template": call_template,
        "result_template": result_template,
        "idempotent_hint": idempotent_hint,
        "read_only_hint": read_only_hint,
        "kind": kind,
    })
}

pub fn format_tool_declaration(
    tool: &ToolDeclaration,
    format: ToolOutputFormat,
) -> serde_json::Result<String> {
    let mut value = tool_declaration_value(tool);
    match format {
        ToolOutputFormat::Json => serde_json::to_string_pretty(&value),
        ToolOutputFormat::Human => {
            let metadata = value.as_object_mut().expect("declaration is an object");
            metadata.remove("name");
            metadata.remove("description");
            metadata.remove("parameters");
            // Don't flatten properties: unions, recursive refs, array items and
            // schema-level constraints would disappear from that presentation.
            Ok(format!(
                "Tool: {}\n{}\n\nMetadata:\n{}\n\nInput schema:\n{}",
                tool.name,
                tool.description,
                serde_json::to_string_pretty(&value)?,
                serde_json::to_string_pretty(tool.parameters.as_value())?,
            ))
        }
    }
}

pub fn format_tool_list(
    tools: &[ToolDeclaration],
    format: ToolOutputFormat,
) -> serde_json::Result<String> {
    match format {
        ToolOutputFormat::Json => serde_json::to_string_pretty(
            &tools.iter().map(tool_declaration_value).collect::<Vec<_>>(),
        ),
        ToolOutputFormat::Human if tools.is_empty() => Ok("No tools found.".to_owned()),
        ToolOutputFormat::Human => tools
            .iter()
            .map(|tool| format_tool_declaration(tool, format))
            .collect::<serde_json::Result<Vec<_>>>()
            .map(|entries| entries.join("\n\n")),
    }
}

/// Preserve all content variants, structured data, error/partial markers and
/// extension fields. Formatting doesn't decide whether execution succeeded.
pub fn format_tool_result(result: &Value, format: ToolOutputFormat) -> serde_json::Result<String> {
    match (format, result) {
        (ToolOutputFormat::Human, Value::String(text)) => Ok(text.clone()),
        _ => serde_json::to_string_pretty(result),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::{event::ToolKind, json_schema::JsonSchema};

    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: "fs.inspect".into(),
            description: "Inspect nested input.\nKeeps the complete schema.".into(),
            parameters: JsonSchema::new(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "$defs": {"node": {
                    "type": "object",
                    "properties": {"next": {"anyOf": [{"$ref": "#/$defs/node"}, {"type": "null"}]}},
                    "additionalProperties": false
                }},
                "type": "object",
                "properties": {
                    "nodes": {"type": "array", "items": {"$ref": "#/$defs/node"}, "minItems": 1},
                    "mode": {"oneOf": [{"enum": ["read", "write"]}, {"type": "null"}]},
                    "bounds": {"allOf": [{"type": "number", "minimum": 0}, {"maximum": 10}]},
                    "headers": {"type": "object", "additionalProperties": {"type": "string"}}
                },
                "required": ["nodes"],
                "additionalProperties": false,
                "x-server-extension": {"preserve": true}
            })),
            mcp_tool_name: Some("inspect".into()),
            mcp_server_name: Some("fs".into()),
            call_template: Some("{{ args | tojson }}".into()),
            result_template: Some("{{ result | tojson }}".into()),
            idempotent_hint: Some(true),
            read_only_hint: Some(false),
            kind: Some(ToolKind::Read),
        }
    }

    #[test]
    fn declaration_json_contains_every_metadata_field_and_full_schema() {
        let tool = declaration();
        let value: Value =
            serde_json::from_str(&format_tool_declaration(&tool, ToolOutputFormat::Json).unwrap())
                .unwrap();
        assert_eq!(
            value,
            json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters.as_value(),
                "mcp_tool_name": "inspect",
                "mcp_server_name": "fs",
                "call_template": "{{ args | tojson }}",
                "result_template": "{{ result | tojson }}",
                "idempotent_hint": true,
                "read_only_hint": false,
                "kind": "Read"
            })
        );
        // Inspection must not change persistence serialization or the source schema.
        assert!(serde_json::to_value(&tool)
            .unwrap()
            .get("mcp_tool_name")
            .is_none());
        assert_eq!(&value["parameters"], tool.parameters.as_value());
    }

    #[test]
    fn human_declaration_keeps_complete_complex_schema_and_metadata() {
        let tool = declaration();
        let out = format_tool_declaration(&tool, ToolOutputFormat::Human).unwrap();
        let (header, schema) = out.split_once("\n\nInput schema:\n").unwrap();
        assert!(header.starts_with(&format!("Tool: {}\n{}", tool.name, tool.description)));
        let (_, metadata) = header.split_once("\n\nMetadata:\n").unwrap();
        let metadata: Value = serde_json::from_str(metadata).unwrap();
        assert_eq!(
            metadata,
            json!({
                "mcp_tool_name": "inspect", "mcp_server_name": "fs",
                "call_template": "{{ args | tojson }}", "result_template": "{{ result | tojson }}",
                "idempotent_hint": true, "read_only_hint": false, "kind": "Read"
            })
        );
        assert_eq!(
            serde_json::from_str::<Value>(schema).unwrap(),
            *tool.parameters.as_value()
        );
        assert!(schema.contains('\n'));
    }

    #[test]
    fn list_retains_full_declarations_and_empty_lists_are_explicit() {
        let tool = declaration();
        let tools = [tool.clone(), tool.clone()];
        let out = format_tool_list(&tools, ToolOutputFormat::Json).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap(),
            json!([tool_declaration_value(&tool), tool_declaration_value(&tool)])
        );
        let entry = format_tool_declaration(&tool, ToolOutputFormat::Human).unwrap();
        assert_eq!(
            format_tool_list(&tools, ToolOutputFormat::Human).unwrap(),
            format!("{entry}\n\n{entry}")
        );
        assert_eq!(format_tool_list(&[], ToolOutputFormat::Json).unwrap(), "[]");
        assert_eq!(
            format_tool_list(&[], ToolOutputFormat::Human).unwrap(),
            "No tools found."
        );
    }

    #[test]
    fn absent_metadata_and_non_object_schemas_are_not_rewritten() {
        let mut tool = declaration();
        tool.mcp_tool_name = None;
        tool.mcp_server_name = None;
        tool.call_template = None;
        tool.result_template = None;
        tool.idempotent_hint = None;
        tool.read_only_hint = None;
        tool.kind = None;
        tool.parameters = JsonSchema::new(json!(false));
        let value = tool_declaration_value(&tool);
        for field in [
            "mcp_tool_name",
            "mcp_server_name",
            "call_template",
            "result_template",
            "idempotent_hint",
            "read_only_hint",
            "kind",
        ] {
            assert_eq!(value[field], Value::Null);
        }
        assert_eq!(value["parameters"], false);
        assert!(format_tool_declaration(&tool, ToolOutputFormat::Human)
            .unwrap()
            .ends_with("\nfalse"));
    }

    #[test]
    fn result_keeps_mixed_content_structured_data_errors_and_extensions() {
        let result = json!({
            "content": [
                {"type": "text", "text": "hello\n\"quoted\""},
                {"type": "image", "data": "aW1hZ2U=", "mimeType": "image/png"},
                {"type": "audio", "data": "YXVkaW8=", "mimeType": "audio/wav"},
                {"type": "resource", "resource": {"uri": "file:///tmp/data", "text": "resource body"}},
                {"type": "resource_link", "uri": "cid:media:owner/session/hash", "name": "attachment"},
                {"type": "future_content", "nested": {"value": [1, null]}}
            ],
            "structuredContent": {"records": [{"name": "quoted name", "ok": false}]},
            "isError": true,
            "partial": true,
            "_meta": {"cursor": "resume-here"},
            "futureField": {"retain": true}
        });
        for format in [ToolOutputFormat::Human, ToolOutputFormat::Json] {
            let out = format_tool_result(&result, format).unwrap();
            assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), result);
            assert!(out.contains('\n'));
        }
    }

    #[test]
    fn result_handles_structured_only_and_scalar_outputs_without_loss() {
        for result in [
            json!({"structuredContent": {"ok": true}}),
            json!([1, null]),
            json!(null),
            json!(true),
        ] {
            for format in [ToolOutputFormat::Human, ToolOutputFormat::Json] {
                let out = format_tool_result(&result, format).unwrap();
                assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), result);
            }
        }
        let result = json!("a \"quoted\" string\nwith spaces");
        assert_eq!(
            format_tool_result(&result, ToolOutputFormat::Human).unwrap(),
            result.as_str().unwrap()
        );
        assert_eq!(
            serde_json::from_str::<Value>(
                &format_tool_result(&result, ToolOutputFormat::Json).unwrap()
            )
            .unwrap(),
            result
        );
        assert_eq!(ToolOutputFormat::default(), ToolOutputFormat::Human);
        assert_eq!(
            ToolOutputFormat::from_json_flag(false),
            ToolOutputFormat::Human
        );
        assert_eq!(
            ToolOutputFormat::from_json_flag(true),
            ToolOutputFormat::Json
        );
    }
}
