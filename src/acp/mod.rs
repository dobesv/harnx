#![allow(dead_code)]

mod client;
mod config;
mod server;

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

fn required_string<'a>(
    arguments: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<&'a str> {
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
#[allow(unused_imports)]
pub use server::HarnxAgent;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn run_async<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime")
            .block_on(future)
    }

    fn test_config(name: &str) -> AcpServerConfig {
        AcpServerConfig {
            name: name.to_string(),
            command: "dummy".to_string(),
            args: vec![],
            env: HashMap::new(),
            enabled: true,
            description: None,
        }
    }

    #[test]
    fn test_acp_config_deserialize_full() {
        let yaml = r#"
name: test-agent
command: /usr/bin/test
args: ["--verbose"]
env:
  KEY: value
enabled: true
description: A test agent
"#;

        let config: AcpServerConfig =
            serde_yaml::from_str(yaml).expect("deserialize full ACP config");

        assert_eq!(config.name, "test-agent");
        assert_eq!(config.command, "/usr/bin/test");
        assert_eq!(config.args, vec!["--verbose"]);
        assert_eq!(config.env.get("KEY").map(String::as_str), Some("value"));
        assert!(config.enabled);
        assert_eq!(config.description.as_deref(), Some("A test agent"));
    }

    #[test]
    fn test_acp_config_deserialize_defaults() {
        let yaml = r#"
name: minimal
command: agent
"#;

        let config: AcpServerConfig =
            serde_yaml::from_str(yaml).expect("deserialize minimal ACP config");

        assert_eq!(config.name, "minimal");
        assert_eq!(config.command, "agent");
        assert!(config.args.is_empty());
        assert!(config.env.is_empty());
        assert!(config.enabled);
        assert!(config.description.is_none());
    }

    #[test]
    fn test_acp_config_disabled() {
        let yaml = r#"
name: disabled-agent
command: agent
enabled: false
"#;

        let config: AcpServerConfig =
            serde_yaml::from_str(yaml).expect("deserialize disabled ACP config");

        assert!(!config.enabled);
    }

    #[test]
    fn test_generate_acp_tools() {
        let tools = generate_acp_tools("myagent");

        assert_eq!(tools.len(), 4);

        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert!(names.contains(&"myagent_session_new"));
        assert!(names.contains(&"myagent_session_prompt"));
        assert!(names.contains(&"myagent_session_load"));
        assert!(names.contains(&"myagent_session_cancel"));
    }

    #[test]
    fn test_session_prompt_tool_has_message_param() {
        let tools = generate_acp_tools("agent1");
        let prompt_tool = tools
            .iter()
            .find(|tool| tool.name == "agent1_session_prompt")
            .expect("find prompt tool");

        let props = prompt_tool
            .parameters
            .properties
            .as_ref()
            .expect("prompt tool properties");
        assert!(props.contains_key("message"));
        assert!(props.contains_key("session_id"));

        let required = prompt_tool
            .parameters
            .required
            .as_ref()
            .expect("prompt tool required list");
        assert!(required.contains(&"message".to_string()));
    }

    #[test]
    fn test_session_cancel_tool_requires_session_id() {
        let tools = generate_acp_tools("agent1");
        let cancel_tool = tools
            .iter()
            .find(|tool| tool.name == "agent1_session_cancel")
            .expect("find cancel tool");

        let required = cancel_tool
            .parameters
            .required
            .as_ref()
            .expect("cancel tool required list");
        assert!(required.contains(&"session_id".to_string()));
    }

    #[test]
    fn test_find_client_for_tool_matches() {
        let manager = AcpManager::new();
        manager.initialize(vec![test_config("myserver")]);

        let result = manager.find_client_for_tool("myserver_session_new");

        assert!(result.is_some());
        let (client, method) = result.expect("matched client for tool");
        assert_eq!(client.name(), "myserver");
        assert_eq!(method, "session_new");
    }

    #[test]
    fn test_find_client_for_tool_no_match() {
        let manager = AcpManager::new();
        manager.initialize(vec![]);

        assert!(manager
            .find_client_for_tool("unknown_session_new")
            .is_none());
    }

    #[test]
    fn test_find_client_for_tool_wrong_method() {
        let manager = AcpManager::new();
        manager.initialize(vec![test_config("srv")]);

        assert!(manager.find_client_for_tool("srv_unknown_method").is_none());
    }

    #[test]
    fn test_get_all_tools_blocking_multiple_servers() {
        let manager = AcpManager::new();
        manager.initialize(vec![test_config("a"), test_config("b")]);

        let tools = manager.get_all_tools_blocking();

        assert_eq!(tools.len(), 8);
    }

    #[test]
    fn test_disabled_server_not_initialized() {
        let manager = AcpManager::new();
        let mut config = test_config("disabled");
        config.enabled = false;
        manager.initialize(vec![config]);

        assert!(manager.get_client("disabled").is_none());
        assert_eq!(manager.get_all_tools_blocking().len(), 0);
    }

    #[test]
    fn test_call_tool_session_prompt_validates_message_argument() {
        let manager = AcpManager::new();
        manager.initialize(vec![test_config("srv")]);

        let err =
            run_async(manager.call_tool("srv_session_prompt", json!({ "session_id": "abc" })))
                .expect_err("missing message should fail before ACP subprocess work");

        assert!(err.to_string().contains("message"));
    }

    #[test]
    fn test_call_tool_session_cancel_validates_session_id_argument_type() {
        let manager = AcpManager::new();
        manager.initialize(vec![test_config("srv")]);

        let err = run_async(manager.call_tool("srv_session_cancel", json!({ "session_id": 123 })))
            .expect_err("invalid session_id type should fail before ACP subprocess work");

        assert!(err.to_string().contains("session_id"));
    }
}
