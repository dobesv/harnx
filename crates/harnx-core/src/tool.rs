//! Tool data types (schemas and invocations) shared across the client,
//! engine, and future MCP/ACP layers. The engine-level logic that
//! actually evaluates tool calls lives in `harnx/src/tool.rs`; this
//! module is deliberately side-effect-free so it can be linked into any
//! crate that needs to speak the schema.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolResult {
    pub call: ToolCall,
    pub output: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch_agent: Option<SwitchAgentData>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchAgentData {
    pub agent: String,
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl ToolResult {
    pub fn new(call: ToolCall, output: Value) -> Self {
        Self {
            call,
            output,
            switch_agent: None,
        }
    }
}

pub enum ToolError {
    Recoverable(anyhow::Error),
    Fatal(anyhow::Error),
}

#[derive(Debug, Clone, Default)]
pub struct Tools {
    declarations: Vec<ToolDeclaration>,
}

impl Tools {
    pub fn init_from_mcp(mcp_tools: Option<Vec<ToolDeclaration>>) -> Self {
        Self {
            declarations: mcp_tools.unwrap_or_default(),
        }
    }

    pub fn declarations(&self) -> Vec<ToolDeclaration> {
        self.declarations.clone()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDeclaration {
    pub name: String,
    pub description: String,
    pub parameters: JsonSchema,
    #[serde(skip, default)]
    pub mcp_tool_name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JsonSchema {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<IndexMap<String, JsonSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<JsonSchema>>,
    #[serde(rename = "anyOf", skip_serializing_if = "Option::is_none")]
    pub any_of: Option<Vec<JsonSchema>>,
    #[serde(rename = "enum", skip_serializing_if = "Option::is_none")]
    pub enum_value: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<Vec<String>>,
}

impl JsonSchema {
    pub fn is_empty_properties(&self) -> bool {
        match &self.properties {
            Some(v) => v.is_empty(),
            None => true,
        }
    }

    /// Simplify the schema for providers that don't support `anyOf` (e.g. Gemini).
    ///
    /// * `anyOf: [<schema>, {"type":"null"}]` → the non-null schema (makes
    ///   `Option<T>` transparent).
    /// * Recursively applied to `properties` and `items`.
    pub fn flatten_any_of(mut self) -> Self {
        // Resolve top-level anyOf with a single non-null variant
        if let Some(variants) = self.any_of.take() {
            let non_null: Vec<JsonSchema> = variants
                .into_iter()
                .filter(|v| v.type_value.as_deref() != Some("null"))
                .collect();
            if non_null.len() == 1 {
                let mut inner = non_null.into_iter().next().unwrap().flatten_any_of();
                // Preserve description from the outer schema if the inner one lacks it
                if inner.description.is_none() {
                    inner.description = self.description;
                }
                return inner;
            }
            // Put back if we can't simplify
            self.any_of = Some(non_null.into_iter().map(|v| v.flatten_any_of()).collect());
        }

        // Recurse into properties
        if let Some(properties) = self.properties.take() {
            self.properties = Some(
                properties
                    .into_iter()
                    .map(|(k, v)| (k, v.flatten_any_of()))
                    .collect(),
            );
        }

        // Recurse into items
        if let Some(items) = self.items.take() {
            self.items = Some(Box::new((*items).flatten_any_of()));
        }

        self
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

impl ToolCall {
    pub fn dedup(calls: Vec<Self>) -> Vec<Self> {
        let mut new_calls = vec![];
        let mut seen_ids = HashSet::new();

        for call in calls.into_iter().rev() {
            if let Some(id) = &call.id {
                if !seen_ids.contains(id) {
                    seen_ids.insert(id.clone());
                    new_calls.push(call);
                }
            } else {
                new_calls.push(call);
            }
        }

        new_calls.reverse();
        new_calls
    }

    pub fn new(
        name: String,
        arguments: Value,
        id: Option<String>,
        thought_signature: Option<String>,
    ) -> Self {
        Self {
            name,
            arguments,
            id,
            thought_signature,
        }
    }
}

pub const TRIGGER_AGENT_TOOL_NAME: &str = "trigger_agent";

/// Builds the deprecated `trigger_agent` tool declaration for
/// backward-compatible agent handoff. Prefer per-agent `*_session_handoff`
/// tools for interactive delegation; this declaration is kept for older
/// configs that still reference it by name.
pub fn trigger_agent_tool_declaration() -> ToolDeclaration {
    let mut properties = IndexMap::new();
    properties.insert(
        "agent".to_string(),
        JsonSchema {
            type_value: Some("string".to_string()),
            description: Some("The name of the agent to transfer the session to.".to_string()),
            ..Default::default()
        },
    );
    properties.insert(
        "prompt".to_string(),
        JsonSchema {
            type_value: Some("string".to_string()),
            description: Some("The new prompt to start the new agent with.".to_string()),
            ..Default::default()
        },
    );
    ToolDeclaration {
        name: TRIGGER_AGENT_TOOL_NAME.to_string(),
        description: "Deprecated compatibility tool. Prefer per-agent *_session_handoff tools for interactive delegation."
            .to_string(),
        parameters: JsonSchema {
            type_value: Some("object".to_string()),
            properties: Some(properties),
            required: Some(vec!["agent".to_string(), "prompt".to_string()]),
            ..Default::default()
        },
        mcp_tool_name: None,
    }
}

/// Extracts user-visible text from an MCP `CallToolResult` value.
///
/// The result value has the shape:
/// ```json
/// { "content": [{ "type": "text", "text": "...", "annotations": { "audience": ["user"] } }] }
/// ```
///
/// Content parts whose `annotations.audience` exists but does NOT contain
/// `"user"` are skipped. Parts with no annotations or with audience
/// containing `"user"` are included. Returns `Some(joined_text)` if any
/// text was extracted, `None` otherwise.
pub fn extract_user_display_text(result: &Value) -> Option<String> {
    let content = result.get("content")?.as_array()?;
    let mut parts: Vec<&str> = Vec::new();
    for item in content {
        // Check audience annotation: if present and does NOT contain "user", skip
        if let Some(annotations) = item.get("annotations") {
            if let Some(audience) = annotations.get("audience") {
                if let Some(audience_arr) = audience.as_array() {
                    if !audience_arr.iter().any(|v| v.as_str() == Some("user")) {
                        continue;
                    }
                }
            }
        }
        // Extract text from this content part
        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
            parts.push(text);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_user_display_text_basic() {
        let result = json!({
            "content": [
                {"type": "text", "text": "hello world"}
            ],
            "isError": false
        });
        assert_eq!(
            extract_user_display_text(&result),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn test_extract_user_display_text_multiple_parts() {
        let result = json!({
            "content": [
                {"type": "text", "text": "line one"},
                {"type": "text", "text": "line two"}
            ]
        });
        assert_eq!(
            extract_user_display_text(&result),
            Some("line one\nline two".to_string())
        );
    }

    #[test]
    fn test_extract_user_display_text_user_audience() {
        let result = json!({
            "content": [
                {
                    "type": "text",
                    "text": "visible",
                    "annotations": {"audience": ["user"]}
                }
            ]
        });
        assert_eq!(
            extract_user_display_text(&result),
            Some("visible".to_string())
        );
    }

    #[test]
    fn test_extract_user_display_text_assistant_only_audience() {
        let result = json!({
            "content": [
                {
                    "type": "text",
                    "text": "hidden from display",
                    "annotations": {"audience": ["assistant"]}
                }
            ]
        });
        assert_eq!(extract_user_display_text(&result), None);
    }

    #[test]
    fn test_extract_user_display_text_mixed_audiences() {
        let result = json!({
            "content": [
                {
                    "type": "text",
                    "text": "user sees this",
                    "annotations": {"audience": ["user"]}
                },
                {
                    "type": "text",
                    "text": "assistant only",
                    "annotations": {"audience": ["assistant"]}
                },
                {
                    "type": "text",
                    "text": "no annotations"
                }
            ]
        });
        assert_eq!(
            extract_user_display_text(&result),
            Some("user sees this\nno annotations".to_string())
        );
    }

    #[test]
    fn test_extract_user_display_text_both_audiences() {
        let result = json!({
            "content": [
                {
                    "type": "text",
                    "text": "for both",
                    "annotations": {"audience": ["user", "assistant"]}
                }
            ]
        });
        assert_eq!(
            extract_user_display_text(&result),
            Some("for both".to_string())
        );
    }

    #[test]
    fn test_extract_user_display_text_no_content() {
        let result = json!({"isError": false});
        assert_eq!(extract_user_display_text(&result), None);
    }

    #[test]
    fn test_extract_user_display_text_empty_content() {
        let result = json!({"content": []});
        assert_eq!(extract_user_display_text(&result), None);
    }

    #[test]
    fn test_extract_user_display_text_non_mcp_value() {
        // Plain string value — not MCP format
        let result = json!("just a string");
        assert_eq!(extract_user_display_text(&result), None);
    }

    #[test]
    fn test_extract_user_display_text_content_without_text() {
        let result = json!({
            "content": [
                {"type": "image", "data": "base64..."}
            ]
        });
        assert_eq!(extract_user_display_text(&result), None);
    }

    #[test]
    fn test_extract_user_display_text_annotations_without_audience() {
        let result = json!({
            "content": [
                {
                    "type": "text",
                    "text": "has annotations but no audience",
                    "annotations": {"priority": 0.5}
                }
            ]
        });
        assert_eq!(
            extract_user_display_text(&result),
            Some("has annotations but no audience".to_string())
        );
    }
}
