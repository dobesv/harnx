mod client;
mod config;

use crate::tool::{JsonSchema, ToolDeclaration};

use anyhow::{anyhow, Result};
use indexmap::IndexMap;
use parking_lot::RwLock;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

pub struct AcpManager {
    clients: Arc<RwLock<HashMap<String, Arc<AcpClient>>>>,
}

impl fmt::Debug for AcpManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcpManager")
            .field("client_count", &self.clients.read().len())
            .finish()
    }
}

impl AcpManager {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn initialize(&self, configs: Vec<AcpServerConfig>) {
        let mut clients = HashMap::new();
        for config in configs.into_iter().filter(|config| config.enabled) {
            clients.insert(config.name.clone(), Arc::new(AcpClient::new(config)));
        }
        *self.clients.write() = clients;
    }

    pub fn get_client(&self, server_name: &str) -> Option<Arc<AcpClient>> {
        self.clients.read().get(server_name).cloned()
    }

    pub async fn get_all_tools(&self) -> Vec<ToolDeclaration> {
        let clients = self.clients.read();
        let mut tools: Vec<_> = clients
            .keys()
            .flat_map(|name| generate_acp_tools(name))
            .collect();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        tools
    }

    pub fn get_all_tools_blocking(&self) -> Vec<ToolDeclaration> {
        let clients = self.clients.read();
        let mut tools: Vec<_> = clients
            .keys()
            .flat_map(|name| generate_acp_tools(name))
            .collect();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        tools
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let (client, method) = self
            .find_client_for_tool(name)
            .ok_or_else(|| anyhow!("Unknown ACP tool '{}'", name))?;

        let arguments = expect_object(arguments)?;

        match method.as_str() {
            "session_new" => {
                let session_id = client.session_new().await?;
                Ok(json!({ "session_id": session_id }))
            }
            "session_prompt" => {
                let message = required_string(&arguments, "message")?.to_owned();
                let session_id = match optional_string(&arguments, "session_id")? {
                    Some(session_id) => session_id.to_owned(),
                    None => client.session_new().await?,
                };
                let response = client.session_prompt(Some(&session_id), &message).await?;
                Ok(json!({
                    "session_id": session_id,
                    "response": response,
                }))
            }
            "session_load" => {
                let session_id = required_string(&arguments, "session_id")?.to_owned();
                client.session_load(&session_id).await?;
                Ok(json!({
                    "session_id": session_id,
                    "loaded": true,
                }))
            }
            "session_cancel" => {
                let session_id = required_string(&arguments, "session_id")?.to_owned();
                client.session_cancel(&session_id).await?;
                Ok(json!({
                    "session_id": session_id,
                    "cancelled": true,
                }))
            }
            _ => Err(anyhow!("Unsupported ACP method '{}'", method)),
        }
    }

    pub fn find_client_for_tool(&self, tool_name: &str) -> Option<(Arc<AcpClient>, String)> {
        let clients = self.clients.read();
        for (name, client) in clients.iter() {
            let prefix = format!("{name}_");
            let Some(method) = tool_name.strip_prefix(&prefix) else {
                continue;
            };
            match method {
                "session_new" | "session_prompt" | "session_load" | "session_cancel" => {
                    return Some((Arc::clone(client), method.to_string()));
                }
                _ => continue,
            }
        }
        None
    }
}

impl Default for AcpManager {
    fn default() -> Self {
        Self::new()
    }
}

fn generate_acp_tools(server_name: &str) -> Vec<ToolDeclaration> {
    vec![
        ToolDeclaration {
            name: format!("{server_name}_session_new"),
            description: format!("Create a new session on the '{server_name}' ACP agent"),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::new()),
                ..Default::default()
            },
            mcp_tool_name: Some("session_new".to_string()),
        },
        ToolDeclaration {
            name: format!("{server_name}_session_prompt"),
            description: format!(
                "Send a prompt to the '{server_name}' ACP agent. Auto-creates a session if session_id is not provided."
            ),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some({
                    let mut props = IndexMap::new();
                    props.insert(
                        "message".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("The prompt message to send to the agent".to_string()),
                            ..Default::default()
                        },
                    );
                    props.insert(
                        "session_id".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some(
                                "Session ID from a previous session_new call. If omitted, a new session is created automatically.".to_string(),
                            ),
                            ..Default::default()
                        },
                    );
                    props
                }),
                required: Some(vec!["message".to_string()]),
                ..Default::default()
            },
            mcp_tool_name: Some("session_prompt".to_string()),
        },
        ToolDeclaration {
            name: format!("{server_name}_session_load"),
            description: format!("Load an existing session on the '{server_name}' ACP agent"),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some({
                    let mut props = IndexMap::new();
                    props.insert(
                        "session_id".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("The session ID to load".to_string()),
                            ..Default::default()
                        },
                    );
                    props
                }),
                required: Some(vec!["session_id".to_string()]),
                ..Default::default()
            },
            mcp_tool_name: Some("session_load".to_string()),
        },
        ToolDeclaration {
            name: format!("{server_name}_session_cancel"),
            description: format!("Cancel a running prompt on the '{server_name}' ACP agent"),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some({
                    let mut props = IndexMap::new();
                    props.insert(
                        "session_id".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("The session ID to cancel".to_string()),
                            ..Default::default()
                        },
                    );
                    props
                }),
                required: Some(vec!["session_id".to_string()]),
                ..Default::default()
            },
            mcp_tool_name: Some("session_cancel".to_string()),
        },
    ]
}

fn expect_object(arguments: Value) -> Result<serde_json::Map<String, Value>> {
    match arguments {
        Value::Object(arguments) => Ok(arguments),
        _ => Err(anyhow!("ACP tool arguments must be a JSON object")),
    }
}

fn required_string<'a>(arguments: &'a serde_json::Map<String, Value>, key: &str) -> Result<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("ACP tool argument '{}' must be a string", key))
}

fn optional_string<'a>(
    arguments: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>> {
    match arguments.get(key) {
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(anyhow!("ACP tool argument '{}' must be a string", key)),
    }
}

pub use client::AcpClient;
pub use config::AcpServerConfig;
