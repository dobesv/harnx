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
