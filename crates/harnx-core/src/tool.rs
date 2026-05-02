//! Tool data types (schemas and invocations) shared across the client,
//! engine, and future MCP/ACP layers. The engine-level logic that
//! actually evaluates tool calls lives in `harnx/src/tool.rs`; this
//! module is deliberately side-effect-free so it can be linked into any
//! crate that needs to speak the schema.

use crate::abort::AbortSignal;
use async_trait::async_trait;
use indexmap::IndexMap;
use minijinja::{Environment, Error, UndefinedBehavior};
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

/// Dispatch interface for external tool providers (MCP, ACP, or any
/// future transport). Implementations encapsulate their own client
/// lookup, abort-racing, and UI plumbing — the tool-call loop only
/// asks "do you handle this tool?" and "call it".
#[async_trait]
pub trait ToolProvider: Send + Sync {
    /// Short provider identifier used for logging/diagnostics.
    fn name(&self) -> &str;

    /// Returns true if this provider knows how to dispatch `tool_name`.
    /// Used by the routing loop to decide which provider gets the call.
    fn has_tool(&self, tool_name: &str) -> bool;

    /// Dispatches the tool. Races the call against `abort`. On success
    /// returns the tool's result JSON. On abort, returns a
    /// `ToolError::Fatal` so the outer batch short-circuits. On other
    /// failures (timeouts, bad args, connection errors) returns
    /// `ToolError::Recoverable` so the LLM sees the error and can retry.
    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
        abort: &AbortSignal,
    ) -> Result<Value, ToolError>;
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
    #[serde(skip, default)]
    pub call_template: Option<String>,
    #[serde(skip, default)]
    pub result_template: Option<String>,
}

fn make_template_env<'a>() -> Environment<'a> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Lenient);
    // `truncate` is not a minijinja built-in; register it so templates can do
    // `{{ args.message | truncate(60) }}` or `{{ args.id | truncate(8, end='') }}`.
    env.add_filter(
        "truncate",
        |value: &str, length: usize, end: Option<&str>| -> String {
            let ellipsis = end.unwrap_or("...");
            if value.len() <= length {
                value.to_string()
            } else {
                let cut = length.saturating_sub(ellipsis.len());
                format!("{}{}", &value[..cut], ellipsis)
            }
        },
    );
    env
}

/// Render a MiniJinja call template.
/// `raw_fallback` is default display string (YAML of args) — available as `raw()` in template.
/// Returns `Err(minijinja::Error)` on failure so callers can log error.
pub fn render_tool_call_template(
    template: &str,
    args: &serde_json::Value,
    raw_fallback: &str,
) -> Result<String, Error> {
    let raw = raw_fallback.to_string();
    let mut env = make_template_env();
    env.add_function("raw", move || Ok(raw.clone()));
    let ctx = serde_json::json!({ "args": args });
    env.render_str(template, ctx)
}

/// Render a MiniJinja result template.
/// `raw_fallback` is default display string (extract_user_display_text → YAML) — available as `raw()` in template.
/// Returns `Err(minijinja::Error)` on failure so callers can log error.
pub fn render_tool_result_template(
    template: &str,
    result: &serde_json::Value,
    raw_fallback: &str,
) -> Result<String, Error> {
    let raw = raw_fallback.to_string();
    let mut env = make_template_env();
    env.add_function("raw", move || Ok(raw.clone()));
    let ctx = serde_json::json!({ "result": result });
    env.render_str(template, ctx)
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

    #[test]
    fn test_render_tool_call_template_basic() {
        let result = render_tool_call_template(
            "{{ args.command }}",
            &serde_json::json!({"command": "ls"}),
            "fallback",
        );
        assert_eq!(result.unwrap(), "ls");
    }

    #[test]
    fn test_render_tool_call_template_nested() {
        let result = render_tool_call_template(
            "{{ args.path }}",
            &serde_json::json!({"path": "/tmp"}),
            "fallback",
        );
        assert_eq!(result.unwrap(), "/tmp");
    }

    #[test]
    fn test_render_tool_call_template_missing_var() {
        // Lenient mode: missing variable renders as empty string, not an error
        let result =
            render_tool_call_template("{{ args.missing }}", &serde_json::json!({}), "fallback");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "");
    }

    #[test]
    fn test_render_tool_call_template_with_raw_fallback() {
        let result = render_tool_call_template(
            "{{ raw() }}",
            &serde_json::json!({"command": "ls"}),
            "fallback yaml",
        );
        assert_eq!(result.unwrap(), "fallback yaml");
    }

    #[test]
    fn test_render_tool_call_template_default_with_raw() {
        let result = render_tool_call_template(
            "{{ args.command | default(raw()) }}",
            &serde_json::json!({"command": "ls -la"}),
            "fallback",
        );
        assert_eq!(result.unwrap(), "ls -la");
    }

    #[test]
    fn test_render_tool_call_template_missing_uses_raw() {
        let result = render_tool_call_template(
            "{{ args.missing | default(raw()) }}",
            &serde_json::json!({}),
            "yaml fallback",
        );
        assert_eq!(result.unwrap(), "yaml fallback");
    }

    #[test]
    fn test_render_tool_call_template_returns_err_on_bad_syntax() {
        let result = render_tool_call_template(
            "{{ args.command",
            &serde_json::json!({"command": "ls"}),
            "fallback",
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_render_tool_result_template_basic() {
        let result_val = serde_json::json!({"content": [{"text": "hello world"}]});
        let rendered =
            render_tool_result_template("{{ result.content[0].text }}", &result_val, "fallback");
        assert_eq!(rendered.unwrap(), "hello world");
    }

    #[test]
    fn test_render_tool_result_template_with_conditional() {
        let result_val = serde_json::json!({"content": [{"text": "output"}], "isError": false});
        let rendered = render_tool_result_template(
            "{% if result.isError %}ERROR: {% endif %}{{ result.content[0].text | default(raw()) }}",
            &result_val,
            "raw fallback",
        );
        assert_eq!(rendered.unwrap(), "output");
    }

    #[test]
    fn test_render_tool_result_template_error_flag() {
        let result_val = serde_json::json!({"content": [{"text": "boom"}], "isError": true});
        let rendered = render_tool_result_template(
            "{% if result.isError %}ERROR: {% endif %}{{ result.content[0].text | default(raw()) }}",
            &result_val,
            "raw fallback",
        );
        assert_eq!(rendered.unwrap(), "ERROR: boom");
    }
}
