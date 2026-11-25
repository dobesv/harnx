//! Reserved management names and upstream tool conversion.

use harnx_toolset::ToolSpec;
use serde_json::Value;
#[cfg(test)]
use std::sync::Arc;

/// Reserved raw names (decision 4): owned by the management tools, which the
/// worker renders as `<server>_connect`, `<server>_disconnect`, …
pub const RESERVED_TOOL_NAMES: [&str; 3] = ["connect", "disconnect", "credential_status"];

/// Which upstream tools survive into a catalog after reserved-name filtering.
pub fn filter_reserved(tools: Vec<rmcp::model::Tool>) -> (Vec<rmcp::model::Tool>, Vec<String>) {
    let mut kept = Vec::new();
    let mut skipped = Vec::new();
    for tool in tools {
        if RESERVED_TOOL_NAMES.contains(&&*tool.name) {
            skipped.push(tool.name.to_string());
        } else {
            kept.push(tool);
        }
    }
    (kept, skipped)
}

/// Convert one upstream MCP tool into a harnx `ToolSpec` (the bridge's
/// mapping: timeout policy from `_meta`, hints from annotations). Upstream
/// servers that negotiated MRTR may challenge on any tool (SEP-2322 declares
/// the capability server-level), so their tools are stamped
/// `harnx:input_required` to keep the durable challenge path open.
pub fn map_upstream_tool(tool: &rmcp::model::Tool, mrtr_capable: bool) -> ToolSpec {
    let annotations = tool.annotations.as_ref();
    let timeout_secs = tool
        .meta
        .as_ref()
        .and_then(|meta| harnx_toolset::advertised_timeout_secs(&meta.0));
    let spec = ToolSpec {
        cancellation_guarantee: Default::default(),
        name: tool.name.to_string(),
        description: tool
            .description
            .as_ref()
            .map_or_else(String::new, |value| value.to_string()),
        input_schema: Value::Object((*tool.input_schema).clone()),
        idempotent_hint: annotations
            .and_then(|value| value.idempotent_hint)
            .unwrap_or(false),
        read_only_hint: annotations
            .and_then(|value| value.read_only_hint)
            .unwrap_or(false),
        timeout_secs,
        meta: tool.meta.as_ref().map(|meta| meta.0.clone()),
    };
    let spec = match &tool.output_schema {
        Some(schema) => spec.with_summarized_output_schema(&Value::Object((**schema).clone())),
        None => spec,
    };
    if mrtr_capable {
        spec.with_input_required()
    } else {
        spec
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_names_cover_the_management_tools() {
        assert_eq!(
            RESERVED_TOOL_NAMES,
            ["connect", "disconnect", "credential_status"]
        );
        let tool = rmcp::model::Tool::new(
            "connect".to_string(),
            "upstream tool shadowing the connect tool",
            Arc::new(serde_json::Map::new()),
        );
        let (kept, skipped) = filter_reserved(vec![tool]);
        assert!(kept.is_empty());
        assert_eq!(skipped, vec!["connect".to_string()]);
    }

    #[test]
    fn upstream_tool_maps_timeout_hints_and_meta() {
        let mut meta = rmcp::model::MetaObject::default();
        meta.0.insert(
            harnx_toolset::TOOL_TIMEOUT_META_KEY.to_string(),
            Value::from(45u64),
        );
        let mut tool = rmcp::model::Tool::new(
            "search".to_string(),
            "search things",
            Arc::new(serde_json::Map::new()),
        )
        .annotate(
            rmcp::model::ToolAnnotations::new()
                .read_only(true)
                .idempotent(false),
        );
        tool.meta = Some(meta);
        let spec = map_upstream_tool(&tool, false);
        assert_eq!(spec.name, "search");
        assert_eq!(spec.description, "search things");
        assert_eq!(spec.timeout_secs, Some(45));
        assert!(spec.read_only_hint);
        assert!(!spec.idempotent_hint);
        assert!(!spec.requests_input());
        assert!(map_upstream_tool(&tool, true).requests_input());
    }

    fn tool_with_output_schema() -> rmcp::model::Tool {
        let schema = serde_json::json!({"type": "object", "properties": {
            "count": {"type": "integer", "description": "how many\nmore"},
            "inner": {"type": "object", "properties": {"deep": {"type": "string"}}},
        }});
        rmcp::model::Tool::new("echo", "Echo", Arc::new(serde_json::Map::new()))
            .with_raw_output_schema(Arc::new(schema.as_object().expect("object").clone()))
    }

    #[test]
    fn upstream_output_schema_is_copied_as_a_summary() {
        let spec = map_upstream_tool(&tool_with_output_schema(), false);
        assert_eq!(
            spec.output_schema(),
            Some(&serde_json::json!({"type": "object", "properties": {
                "count": {"type": "integer", "description": "how many"},
                "inner": {"type": "object"},
            }}))
        );
    }

    #[test]
    fn upstream_tool_without_output_schema_gets_no_key() {
        let tool = rmcp::model::Tool::new("echo", "Echo", Arc::new(serde_json::Map::new()));
        assert!(map_upstream_tool(&tool, false).output_schema().is_none());
    }

    #[test]
    fn upstream_server_set_output_schema_wins() {
        let own = serde_json::json!({"properties": {"own": {"type": "string", "x": 1}}});
        let mut meta = rmcp::model::MetaObject::default();
        meta.0.insert(
            harnx_toolset::OUTPUT_SCHEMA_META_KEY.to_string(),
            own.clone(),
        );
        let mut tool = tool_with_output_schema();
        tool.meta = Some(meta);
        assert_eq!(map_upstream_tool(&tool, false).output_schema(), Some(&own));
    }
}
