use crate::{
    config::GlobalConfig,
    hooks::{dispatch::dispatch_hooks, HookEvent, HookResultControl},
    utils::*,
};
use harnx::mcp_safety::{truncate_output, TruncateOpts};

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

                if *IS_STDOUT_TERMINAL {
                    let mut opts = TruncateOpts::default();
                    if let Ok((cols, rows)) = crossterm::terminal::size() {
                        opts.head_lines = 5.max((rows / 2) as usize);
                        opts.tail_lines = 0;
                        // Subtracting 4 for "<= " prefix and 1 to be safe
                        opts.line_head_bytes = (cols as usize).saturating_sub(5);
                        opts.line_tail_bytes = 0;
                        opts.marker = Some(" [...] ".to_string());
                    }
                    let output_str = match &result {
                        Value::String(s) => s.clone(),
                        _ => result.to_string(),
                    };
                    let truncated = truncate_output(&output_str, &opts);
                    println!("{}", dimmed_text(&format!("<= {}", truncated)));
                }

                if result.is_null() {
                    result = json!("DONE");
                } else {
                    is_all_null = false;
                }
                let mut result_obj = ToolResult::new(call, result);
                if let Some(obj) = result_obj.output.as_object() {
                    if obj.get("action").and_then(|v| v.as_str()) == Some("switch_agent") {
                        if let (Some(agent), Some(prompt)) = (
                            obj.get("agent").and_then(|v| v.as_str()),
                            obj.get("prompt").and_then(|v| v.as_str()),
                        ) {
                            result_obj.switch_agent = Some(SwitchAgentData {
                                agent: agent.to_string(),
                                prompt: prompt.to_string(),
                            });
                        }
                    }
                }
                output.push(result_obj);
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

                is_all_null = false;
                let error_result = json!({
                    "is_error": true,
                    "error": err.to_string(),
                });
                output.push(ToolResult::new(call, error_result));
            }
        }
    }
    if is_all_null {
        output = vec![];
    }
    Ok(output)
}

pub const TRIGGER_AGENT_TOOL_NAME: &str = "trigger_agent";

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
        description: "Transfer the session to another agent with a new prompt in an empty session."
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
            let prompt = match &json_data {
                Value::Null => format!("Call {}", self.name),
                _ => format!("Call {} {}", self.name, json_data),
            };
            println!("{}", dimmed_text(&prompt));
        }

        if self.name == TRIGGER_AGENT_TOOL_NAME {
            let agent = json_data["agent"]
                .as_str()
                .ok_or_else(|| anyhow!("Missing 'agent' argument for trigger_agent"))?;
            let prompt = json_data["prompt"]
                .as_str()
                .ok_or_else(|| anyhow!("Missing 'prompt' argument for trigger_agent"))?;

            return Ok(json!({
                "status": "success",
                "message": format!("Transferring session to agent '{}'...", agent),
                "action": "switch_agent",
                "agent": agent,
                "prompt": prompt
            }));
        }

        let acp_manager = config.read().acp_manager.clone();
        if let Some(manager) = acp_manager {
            if manager.find_client_for_tool(&self.name).is_some() {
                let tool_name = self.name.clone();
                let result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        tokio::select! {
                            result = manager.call_tool(&tool_name, json_data) => result,
                            _ = tokio::signal::ctrl_c() => {
                                bail!("ACP tool call aborted by user")
                            }
                        }
                    })
                })?;

                return Ok(result);
            }
        }

        let mcp_manager = config.read().mcp_manager.clone();
        let manager = match mcp_manager {
            Some(m) => m,
            None => bail!("No tool provider configured for '{}'", self.name),
        };

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use parking_lot::RwLock;
    use std::sync::Arc;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_eval_tool_calls_error_handling() {
        let config = Arc::new(RwLock::new(Config::default()));
        let call = ToolCall::new("unknown_tool".to_string(), json!({}), Some("1".to_string()));
        let calls = vec![call];

        let result = eval_tool_calls(&config, calls).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].call.name, "unknown_tool");
        assert!(result[0].output.is_object());
        assert_eq!(result[0].output["is_error"], true);
        assert!(result[0].output["error"]
            .as_str()
            .unwrap()
            .contains("No tool provider configured"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_eval_tool_calls_partial_error_handling() {
        let config = Arc::new(RwLock::new(Config::default()));
        // trigger_agent is handled internally and should succeed
        let call1 = ToolCall::new(
            TRIGGER_AGENT_TOOL_NAME.to_string(),
            json!({"agent": "test", "prompt": "test"}),
            Some("1".to_string()),
        );
        let call2 = ToolCall::new("unknown_tool".to_string(), json!({}), Some("2".to_string()));
        let calls = vec![call1, call2];

        let result = eval_tool_calls(&config, calls).unwrap();
        assert_eq!(result.len(), 2);

        assert_eq!(result[0].call.name, TRIGGER_AGENT_TOOL_NAME);
        assert_eq!(result[0].output["action"], "switch_agent");

        assert_eq!(result[1].call.name, "unknown_tool");
        assert_eq!(result[1].output["is_error"], true);
    }
}
