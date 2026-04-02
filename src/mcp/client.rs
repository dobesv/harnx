use super::{mcp_tool_to_declaration, McpServerConfig};
use crate::tool::ToolDeclaration;

use anyhow::{anyhow, bail, Context, Result};
use parking_lot::RwLock;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParam, ClientCapabilities, ErrorData, Implementation, InitializeRequestParam,
    ListRootsResult, ProtocolVersion, Root,
};
use rmcp::service::{RequestContext, RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use std::{collections::HashMap, fmt, path::Path, sync::Arc};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::runtime::{Builder, Handle};

pub struct McpClient {
    name: String,
    config: McpServerConfig,
    tools: Arc<RwLock<Vec<ToolDeclaration>>>,
    roots: Arc<RwLock<Vec<String>>>,
    connected: Arc<RwLock<bool>>,
    connection_failed: Arc<RwLock<bool>>,
    service: Arc<RwLock<Option<RunningService<RoleClient, McpClientHandler>>>>,
}

#[derive(Clone)]
pub struct McpClientHandler {
    roots: Arc<RwLock<Vec<String>>>,
}

impl McpClientHandler {
    pub fn new(roots: Arc<RwLock<Vec<String>>>) -> Self {
        Self { roots }
    }
}

impl ClientHandler for McpClientHandler {
    fn get_info(&self) -> InitializeRequestParam {
        InitializeRequestParam {
            protocol_version: ProtocolVersion::default(),
            capabilities: ClientCapabilities::builder()
                .enable_roots()
                .enable_roots_list_changed()
                .build(),
            client_info: Implementation {
                name: "harnx".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: None,
                website_url: None,
                icons: None,
            },
        }
    }

    async fn list_roots(
        &self,
        _cx: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        let roots = self.roots.read();
        let roots = roots
            .iter()
            .map(|r| {
                let path = Path::new(r);
                let uri = if let Ok(canonical) = path.canonicalize() {
                    format!("file://{}", canonical.to_string_lossy())
                } else {
                    format!("file://{}", r)
                };
                Root { uri, name: None }
            })
            .collect();
        Ok(ListRootsResult { roots })
    }
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
            .field("roots", &*self.roots.read())
            .field("connected", &*self.connected.read())
            .field("service", &service)
            .finish()
    }
}

impl McpClient {
    pub fn new(config: McpServerConfig) -> Self {
        let name = config.name.clone();
        let roots = config.roots.clone();
        Self {
            name,
            config,
            tools: Arc::new(RwLock::new(Vec::new())),
            roots: Arc::new(RwLock::new(roots)),
            connected: Arc::new(RwLock::new(false)),
            connection_failed: Arc::new(RwLock::new(false)),
            service: Arc::new(RwLock::new(None)),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_connected(&self) -> bool {
        *self.connected.read()
    }

    fn connection_failed(&self) -> bool {
        *self.connection_failed.read()
    }

    pub async fn connect(&self) -> Result<()> {
        *self.connection_failed.write() = false;
        if self.is_connected() {
            return Ok(());
        }

        match self.connect_inner().await {
            Ok(()) => Ok(()),
            Err(err) => {
                *self.connection_failed.write() = true;
                Err(err)
            }
        }
    }

    async fn connect_inner(&self) -> Result<()> {
        let mut command = Command::new(&self.config.command);
        command.args(&self.config.args);
        command.envs(&self.config.env);

        let (transport, stderr) = TokioChildProcess::builder(command)
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("Failed to spawn MCP server '{}'", self.name))?;

        if let Some(stderr) = stderr {
            let server_name = self.name.clone();
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    log::debug!("[mcp:{}] {}", server_name, line);
                }
            });
        }

        let handler = McpClientHandler::new(self.roots.clone());
        let service = tokio::time::timeout(
            Duration::from_secs(30),
            rmcp::service::serve_client(handler, transport),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "MCP server '{}' timed out during initialization (30s)",
                self.name
            )
        })?
        .with_context(|| format!("Failed to initialize MCP client for server '{}'", self.name))?;

        let functions = tokio::time::timeout(
            Duration::from_secs(10),
            service.peer().list_tools(Default::default()),
        )
        .await
        .map_err(|_| anyhow!("MCP server '{}' timed out listing tools (10s)", self.name))?
        .with_context(|| format!("Failed to list tools for MCP server '{}'", self.name))?
        .tools
        .into_iter()
        .map(|tool| {
            let input_schema = Value::Object((*tool.input_schema).clone());
            mcp_tool_to_declaration(
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

    pub fn get_tools(&self) -> Vec<ToolDeclaration> {
        self.tools.read().clone()
    }

    pub fn get_roots(&self) -> Vec<String> {
        self.roots.read().clone()
    }

    pub async fn add_root(&self, root: &str) -> Result<()> {
        let changed = {
            let mut roots = self.roots.write();
            if !roots.contains(&root.to_string()) {
                roots.push(root.to_string());
                true
            } else {
                false
            }
        };
        if changed {
            let peer = self.service.read().as_ref().map(|s| s.peer().clone());
            if let Some(peer) = peer {
                let _ = peer.notify_roots_list_changed().await;
            }
        }
        Ok(())
    }

    pub async fn remove_root(&self, root: &str) -> Result<()> {
        let changed = {
            let mut roots = self.roots.write();
            let old_len = roots.len();
            roots.retain(|r| r != root);
            roots.len() < old_len
        };
        if changed {
            let peer = self.service.read().as_ref().map(|s| s.peer().clone());
            if let Some(peer) = peer {
                let _ = peer.notify_roots_list_changed().await;
            }
        }
        Ok(())
    }

    #[allow(clippy::await_holding_lock)]
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
            let service_guard = self.service.read();
            let service = service_guard
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

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::handler::server::ServerHandler;
    use rmcp::model::{InitializeResult, ListToolsResult, ServerCapabilities};
    use rmcp::service::{serve_server, NotificationContext};
    use std::time::Duration;
    use tokio::io::duplex;

    #[derive(Clone, Default, Debug)]
    struct MockServerHandler {
        initialized_params: Arc<RwLock<Option<InitializeRequestParam>>>,
        roots_list_changed_notified: Arc<RwLock<bool>>,
        peer: Arc<RwLock<Option<rmcp::service::Peer<rmcp::service::RoleServer>>>>,
    }

    impl ServerHandler for MockServerHandler {
        fn get_info(&self) -> InitializeResult {
            InitializeResult {
                protocol_version: ProtocolVersion::default(),
                capabilities: ServerCapabilities::default(),
                server_info: Implementation {
                    name: "mock-server".to_string(),
                    version: "0.1.0".to_string(),
                    title: None,
                    website_url: None,
                    icons: None,
                },
                instructions: None,
            }
        }

        async fn initialize(
            &self,
            params: InitializeRequestParam,
            cx: RequestContext<rmcp::service::RoleServer>,
        ) -> Result<InitializeResult, rmcp::model::ErrorData> {
            *self.initialized_params.write() = Some(params);
            *self.peer.write() = Some(cx.peer.clone());
            Ok(self.get_info())
        }

        async fn list_tools(
            &self,
            _params: Option<rmcp::model::PaginatedRequestParam>,
            _cx: RequestContext<rmcp::service::RoleServer>,
        ) -> Result<ListToolsResult, rmcp::model::ErrorData> {
            Ok(ListToolsResult {
                tools: vec![],
                next_cursor: None,
            })
        }

        async fn on_roots_list_changed(&self, _cx: NotificationContext<rmcp::service::RoleServer>) {
            *self.roots_list_changed_notified.write() = true;
        }
    }

    #[tokio::test]
    async fn test_mcp_roots_propagation() {
        let (client_transport, server_transport) = duplex(1024);

        let mock_server = MockServerHandler::default();
        let server_handler = mock_server.clone();

        // Client
        let roots = Arc::new(RwLock::new(vec!["/test/root".to_string()]));
        let handler = McpClientHandler::new(roots.clone());

        let server_fut = serve_server(server_handler, server_transport);
        let client_fut = rmcp::service::serve_client(handler, client_transport);

        let (_server_res, client_res) = tokio::join!(server_fut, client_fut);
        let _client_service = client_res.unwrap();
        let client_peer = _client_service.peer().clone();

        // Run client in background
        let _client_task = tokio::spawn(async move {
            let _ = _client_service.waiting().await;
        });

        // Verify roots in initialize params
        {
            let params = mock_server.initialized_params.read();
            let params = params.as_ref().unwrap();
            assert!(params.capabilities.roots.is_some());
            assert_eq!(
                params.capabilities.roots.as_ref().unwrap().list_changed,
                Some(true)
            );
        }

        let server_peer = mock_server.peer.read().as_ref().unwrap().clone();

        // Verify list_roots works (server calling client)
        let roots_result = server_peer.list_roots().await.unwrap();
        assert_eq!(roots_result.roots.len(), 1);
        assert_eq!(roots_result.roots[0].uri, "file:///test/root");

        // Test adding a root and notification
        {
            roots.write().push("/test/root2".to_string());
            client_peer.notify_roots_list_changed().await.unwrap();
        }

        // Give it a moment to process the notification
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(*mock_server.roots_list_changed_notified.read());

        // Verify new roots
        let roots_result = server_peer.list_roots().await.unwrap();
        assert_eq!(roots_result.roots.len(), 2);
        assert_eq!(roots_result.roots[1].uri, "file:///test/root2");
    }

    #[tokio::test]
    async fn test_mcp_roots_canonicalization() {
        let (client_transport, server_transport) = duplex(1024);
        let mock_server = MockServerHandler::default();
        let server_handler = mock_server.clone();

        // Client with a relative root
        let roots = Arc::new(RwLock::new(vec![".".to_string()]));
        let handler = McpClientHandler::new(roots.clone());

        let server_fut = serve_server(server_handler, server_transport);
        let client_fut = rmcp::service::serve_client(handler, client_transport);

        let (_server_res, client_res) = tokio::join!(server_fut, client_fut);
        let _client_service = client_res.unwrap();

        // Run client in background
        let _client_task = tokio::spawn(async move {
            let _ = _client_service.waiting().await;
        });

        let server_peer = mock_server.peer.read().as_ref().unwrap().clone();

        // Verify list_roots works (server calling client)
        let roots_result = server_peer.list_roots().await.unwrap();
        assert_eq!(roots_result.roots.len(), 1);
        let uri = roots_result.roots[0].uri.clone();

        // It should be an absolute path
        assert!(
            uri.starts_with("file:///"),
            "URI should be an absolute file URI, got: {}",
            uri
        );

        let expected_path = std::env::current_dir().unwrap().canonicalize().unwrap();
        let expected_uri = format!("file://{}", expected_path.to_string_lossy());
        // Note: canonicalize might add \\?\ prefix on Windows, but we are on Linux.
        // Also it should have three slashes for absolute paths: file:///path/to/dir
        // format!("file://{}", path) where path is /path/to/dir gives file:///path/to/dir
        assert_eq!(uri, expected_uri);
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

    pub fn get_client(&self, server_name: &str) -> Option<Arc<McpClient>> {
        self.clients.read().get(server_name).cloned()
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

    pub async fn get_all_tools(&self) -> Vec<ToolDeclaration> {
        let clients: Vec<_> = self.clients.read().values().cloned().collect();
        let connect_futures: Vec<_> = clients
            .iter()
            .filter(|c| !c.is_connected() && !c.connection_failed())
            .map(|client| {
                let client = client.clone();
                async move {
                    if let Err(err) = client.connect().await {
                        eprintln!(
                            "Warning: MCP server '{}' failed to connect: {}. Use '.mcp connect {}' to retry.",
                            client.name(),
                            err,
                            client.name(),
                        );
                        log::warn!(
                            "MCP server '{}' connection failed: {}",
                            client.name(),
                            err,
                        );
                    }
                }
            })
            .collect();
        futures_util::future::join_all(connect_futures).await;

        let mut tools: Vec<_> = clients
            .iter()
            .flat_map(|client| client.get_tools())
            .collect();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        tools
    }

    pub fn get_all_tools_blocking(&self) -> Vec<ToolDeclaration> {
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

    pub async fn get_server_tools(&self, server_name: &str) -> Result<Vec<ToolDeclaration>> {
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
        let (server_name, tool_name) = prefixed_name
            .split_once('_')
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
