use super::{mcp_tool_to_function, McpServerConfig};
use crate::function::FunctionDeclaration;

use anyhow::{anyhow, bail, Context, Result};
use parking_lot::RwLock;
use rmcp::model::CallToolRequestParam;
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::TokioChildProcess;
use serde_json::Value;
use std::{collections::HashMap, fmt, sync::Arc};
use tokio::process::Command;
use tokio::runtime::{Builder, Handle};

pub struct McpClient {
    name: String,
    config: McpServerConfig,
    tools: Arc<RwLock<Vec<FunctionDeclaration>>>,
    connected: Arc<RwLock<bool>>,
    service: Arc<RwLock<Option<RunningService<RoleClient, ()>>>>,
}

impl fmt::Debug for McpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let service = if self.service.read().is_some() {
            "<running-service>"
        } else {
            "<disconnected>"
        };

        f.debug_struct("McpClient")
            .field("name", &self.name)
            .field("config", &self.config)
            .field("tools", &*self.tools.read())
            .field("connected", &*self.connected.read())
            .field("service", &service)
            .finish()
    }
}

impl McpClient {
    pub fn new(config: McpServerConfig) -> Self {
        let name = config.name.clone();
        Self {
            name,
            config,
            tools: Arc::new(RwLock::new(Vec::new())),
            connected: Arc::new(RwLock::new(false)),
            service: Arc::new(RwLock::new(None)),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_connected(&self) -> bool {
        *self.connected.read()
    }

    pub async fn connect(&self) -> Result<()> {
        if self.is_connected() {
            return Ok(());
        }

        let mut command = Command::new(&self.config.command);
        command.args(&self.config.args);
        command.envs(&self.config.env);

        let transport = TokioChildProcess::new(command)
            .with_context(|| format!("Failed to spawn MCP server '{}'", self.name))?;
        let service = ().serve(transport).await.with_context(|| {
            format!("Failed to initialize MCP client for server '{}'", self.name)
        })?;

        let functions = service
            .list_all_tools()
            .await
            .with_context(|| format!("Failed to list tools for MCP server '{}'", self.name))?
            .into_iter()
            .map(|tool| {
                let input_schema = Value::Object((*tool.input_schema).clone());
                mcp_tool_to_function(
                    &self.name,
                    tool.name.as_ref(),
                    tool.description.as_deref().unwrap_or_default(),
                    &input_schema,
                )
            })
            .collect::<Result<Vec<_>>>()?;

        *self.tools.write() = functions;
        *self.connected.write() = true;
        *self.service.write() = Some(service);

        Ok(())
    }

    pub async fn disconnect(&self) -> Result<()> {
        let service = self.service.write().take();

        *self.connected.write() = false;
        self.tools.write().clear();

        if let Some(service) = service {
            service
                .cancel()
                .await
                .with_context(|| format!("Failed to disconnect MCP server '{}'", self.name))?;
        }

        Ok(())
    }

    pub fn get_tools(&self) -> Vec<FunctionDeclaration> {
        self.tools.read().clone()
    }

    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<Value> {
        if !self.is_connected() {
            self.connect().await?;
        }

        let arguments = match arguments {
            Value::Null => None,
            Value::Object(arguments) => Some(arguments),
            _ => bail!("MCP tool arguments must be a JSON object or null"),
        };

        let params = CallToolRequestParam {
            name: tool_name.to_string().into(),
            arguments,
        };

        let result = {
            let service = self.service.read();
            let service = service
                .as_ref()
                .ok_or_else(|| anyhow!("MCP server '{}' is not connected", self.name))?;

            service.call_tool(params).await.with_context(|| {
                format!(
                    "Failed to call tool '{}' on MCP server '{}'",
                    tool_name, self.name
                )
            })?
        };

        serde_json::to_value(result).context("Failed to serialize MCP tool result")
    }
}

#[derive(Debug)]
pub struct McpManager {
    clients: Arc<RwLock<HashMap<String, Arc<McpClient>>>>,
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn initialize(&self, configs: Vec<McpServerConfig>) {
        let mut clients = self.clients.write();
        clients.clear();

        for config in configs.into_iter().filter(|config| config.enabled) {
            clients.insert(config.name.clone(), Arc::new(McpClient::new(config)));
        }
    }

    pub async fn connect(&self, server_name: &str) -> Result<()> {
        let client = self
            .clients
            .read()
            .get(server_name)
            .cloned()
            .ok_or_else(|| anyhow!("Unknown MCP server '{}'", server_name))?;
        client.connect().await
    }


    pub async fn disconnect(&self, server_name: &str) -> Result<()> {
        let client = self
            .clients
            .read()
            .get(server_name)
            .cloned()
            .ok_or_else(|| anyhow!("Unknown MCP server '{}'", server_name))?;
        client.disconnect().await
    }


    pub async fn get_all_tools(&self) -> Vec<FunctionDeclaration> {
        let clients: Vec<_> = self.clients.read().values().cloned().collect();
        for client in &clients {
            if !client.is_connected() {
                if let Err(err) = client.connect().await {
                    log::warn!(
                        "Failed to connect to MCP server '{}': {}",
                        client.name(),
                        err
                    );
                }
            }
        }

        let mut tools: Vec<_> = clients
            .iter()
            .flat_map(|client| client.get_tools())
            .collect();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        tools
    }

    pub fn get_all_tools_blocking(&self) -> Vec<FunctionDeclaration> {
        if let Ok(handle) = Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(self.get_all_tools()))
        } else {
            match Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime.block_on(self.get_all_tools()),
                Err(err) => {
                    log::warn!("Failed to create Tokio runtime for MCP tool discovery: {err}");
                    vec![]
                }
            }
        }
    }

    pub async fn get_server_tools(&self, server_name: &str) -> Result<Vec<FunctionDeclaration>> {
        let client = self
            .clients
            .read()
            .get(server_name)
            .cloned()
            .ok_or_else(|| anyhow!("Unknown MCP server '{}'", server_name))?;

        if !client.is_connected() {
            client.connect().await?;
        }

        Ok(client.get_tools())
    }


    pub async fn call_tool(&self, prefixed_name: &str, arguments: Value) -> Result<Value> {
        let tool_name = prefixed_name
            .strip_prefix("mcp__")
            .ok_or_else(|| anyhow!("Invalid MCP tool name '{}'", prefixed_name))?;
        let (server_name, tool_name) = tool_name
            .split_once("__")
            .ok_or_else(|| anyhow!("Invalid MCP tool name '{}'", prefixed_name))?;

        let client = self
            .clients
            .read()
            .get(server_name)
            .cloned()
            .ok_or_else(|| anyhow!("Unknown MCP server '{}'", server_name))?;

        if !client.is_connected() {
            client.connect().await?;
        }

        client.call_tool(tool_name, arguments).await
    }

    pub fn list_servers(&self) -> Vec<String> {
        let mut servers: Vec<_> = self
            .clients
            .read()
            .values()
            .map(|client| client.name().to_string())
            .collect();
        servers.sort();
        servers
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}
