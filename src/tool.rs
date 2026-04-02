use crate::{
    config::GlobalConfig,
    hooks::{dispatch::dispatch_hooks, HookEvent, HookResultControl},
    utils::*,
};

use anyhow::{anyhow, bail, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use tokio::runtime::Handle;

pub fn eval_tool_calls(config: &GlobalConfig, mut calls: Vec<ToolCall>) -> Result<Vec<ToolResult>> {
    let mut output = vec![];
    if calls.is_empty() {
        return Ok(output);
    }
    calls = ToolCall::dedup(calls);
    if calls.is_empty() {
        bail!("The request was aborted because an infinite loop of function calls was detected.")
    }

    let hooks = config.read().resolved_hooks();
    let cwd = std::env::current_dir().unwrap_or_default();
    let session_id = "cmd".to_string();

    let mut is_all_null = true;
    for call in calls {
        let tool_input = call.arguments.clone();
        let tool_use_id = call.id.clone().unwrap_or_default();

        let pre_event = HookEvent::PreToolUse {
            tool_name: call.name.clone(),
            tool_input: tool_input.clone(),
            tool_use_id: tool_use_id.clone(),
        };
        let pre_outcome = tokio::task::block_in_place(|| {
            Handle::current().block_on(dispatch_hooks(
                &pre_event,
                &hooks.entries,
                &session_id,
                &cwd,
            ))
        });
        if let HookResultControl::Block { reason } = pre_outcome.control {
            let blocked_result = json!({"error": reason, "blocked_by_hook": true});
            output.push(ToolResult::new(call, blocked_result));
            is_all_null = false;
            continue;
        }
        if let HookResultControl::Ask { reason } = pre_outcome.control {
            if !crate::hooks::prompt::confirm_tool_use(
                &call.name,
                &call.arguments,
                reason.as_deref(),
            ) {
                let deny_reason = reason.unwrap_or_else(|| "Denied by user".to_string());
                let blocked_result = json!({"error": deny_reason, "blocked_by_hook": true});
                output.push(ToolResult::new(call, blocked_result));
                is_all_null = false;
                continue;
            }
        }

        let eval_result = call.eval_mcp(config);
        match eval_result {
            Ok(mut result) => {
                let post_event = HookEvent::PostToolUse {
                    tool_name: call.name.clone(),
                    tool_input: tool_input.clone(),
                    tool_response: result.clone(),
                    tool_use_id: tool_use_id.clone(),
                };
                let _ = tokio::task::block_in_place(|| {
                    Handle::current().block_on(dispatch_hooks(
                        &post_event,
                        &hooks.entries,
                        &session_id,
                        &cwd,
                    ))
                });

                if result.is_null() {
                    result = json!("DONE");
                } else {
                    is_all_null = false;
                }
                output.push(ToolResult::new(call, result));
            }
            Err(err) => {
                let fail_event = HookEvent::PostToolUseFailure {
                    tool_name: call.name.clone(),
                    tool_input: tool_input.clone(),
                    tool_use_id: tool_use_id.clone(),
                    error: err.to_string(),
                };
                let _ = tokio::task::block_in_place(|| {
                    Handle::current().block_on(dispatch_hooks(
                        &fail_event,
                        &hooks.entries,
                        &session_id,
                        &cwd,
                    ))
                });

                return Err(err);
            }
        }
    }
    if is_all_null {
        output = vec![];
    }
    Ok(output)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolResult {
    pub call: ToolCall,
    pub output: Value,
}

impl ToolResult {
    pub fn new(call: ToolCall, output: Value) -> Self {
        Self { call, output }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
    pub id: Option<String>,
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

    pub fn new(name: String, arguments: Value, id: Option<String>) -> Self {
        Self {
            name,
            arguments,
            id,
        }
    }

    fn eval_mcp(&self, config: &GlobalConfig) -> Result<Value> {
        let mcp_manager = config.read().mcp_manager.clone();
        let manager = match mcp_manager {
            Some(m) => m,
            None => bail!("MCP is not configured"),
        };

        let json_data = if self.arguments.is_null() {
            Value::Null
        } else if self.arguments.is_object() {
            self.arguments.clone()
        } else if let Some(arguments) = self.arguments.as_str() {
            serde_json::from_str(arguments).map_err(|_| {
                anyhow!(
                    "The call '{}' has invalid arguments: {arguments}",
                    self.name
                )
            })?
        } else {
            bail!(
                "The call '{}' has invalid arguments: {}",
                self.name,
                self.arguments
            );
        };

        if *IS_STDOUT_TERMINAL {
            let prompt = format!("Call {} {}", self.name, json_data);
            println!("{}", dimmed_text(&prompt));
        }

        let tool_name = self.name.clone();
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                tokio::select! {
                    result = manager.call_tool(&tool_name, json_data) => result,
                    _ = tokio::signal::ctrl_c() => {
                        bail!("MCP tool call aborted by user")
                    }
                }
            })
        })?;

        Ok(result)
    }
}
