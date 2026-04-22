use crate::config::McpServerConfig;
use crate::convert::mcp_tool_to_declaration;
use crate::safety::path_to_file_uri;
use harnx_core::abort::{wait_abort_signal, AbortSignal};
use harnx_core::tool::{ToolDeclaration, ToolError, ToolProvider};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use parking_lot::RwLock;
use process_wrap::tokio::CommandWrap;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ErrorData, Implementation, InitializeRequestParams,
    ListRootsResult, Root,
};
use rmcp::service::{RequestContext, RoleClient, RunningService, ServiceError};
use rmcp::transport::TokioChildProcess;
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use std::{collections::HashMap, fmt, path::Path, sync::Arc};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::runtime::{Builder, Handle, RuntimeFlavor};

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
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::builder()
                .enable_roots()
                .enable_roots_list_changed()
                .build(),
            Implementation::new("harnx", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_roots(
        &self,
        _cx: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        let roots = self.roots.read();
        let roots = roots
            .iter()
            .filter_map(|r| {
                let path = Path::new(r);
                let abs = path
                    .canonicalize()
                    .or_else(|_| std::env::current_dir().map(|cwd| cwd.join(path)))
                    .ok()?;
                Some(Root::new(path_to_file_uri(&abs)))
            })
            .collect();
        Ok(ListRootsResult::new(roots))
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
    fn expand_path(path: &str) -> String {
        shellexpand::full(path)
            .map(|p| p.to_string())
            .unwrap_or_else(|_| path.to_string())
    }

    pub fn new(config: McpServerConfig) -> Self {
        let name = config.name.clone();
        let roots = config
            .roots
            .iter()
            .map(|r| Self::expand_path(r))
            .collect::<Vec<_>>();
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

        // Spawn in a new process group so SIGINT (Ctrl+C) in the parent
        // terminal doesn't propagate to MCP server child processes.
        #[allow(unused_mut)]
        let mut wrap = CommandWrap::from(command);
        #[cfg(unix)]
        wrap.wrap(ProcessGroup::leader());

        let (transport, stderr) = TokioChildProcess::builder(wrap)
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
            let server_tool_name = tool.name.to_string();
            let final_name =
                if let Some(renamed) = self.config.rename_tools.get(server_tool_name.as_str()) {
                    renamed.clone()
                } else {
                    format!("{}_{}", self.name, server_tool_name)
                };
            mcp_tool_to_declaration(
                &final_name,
                &server_tool_name,
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

    fn invalidate_service(&self) {
        self.service.write().take();
        *self.connected.write() = false;
    }

    pub fn get_tools(&self) -> Vec<ToolDeclaration> {
        self.tools.read().clone()
    }

    pub fn get_roots(&self) -> Vec<String> {
        self.roots.read().clone()
    }

    pub async fn add_root(&self, root: &str) -> Result<()> {
        let root = Self::expand_path(root);
        let changed = {
            let mut roots = self.roots.write();
            if !roots.contains(&root) {
                roots.push(root);
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

    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<Value> {
        if !self.is_connected() {
            self.connect().await?;
        }

        let arguments = match arguments {
            Value::Null => None,
            Value::Object(arguments) => Some(arguments),
            _ => bail!("MCP tool arguments must be a JSON object or null"),
        };

        let mut params = CallToolRequestParams::new(tool_name.to_string());
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }

        let peer = {
            let service_guard = self.service.read();
            service_guard
                .as_ref()
                .ok_or_else(|| anyhow!("MCP server '{}' is not connected", self.name))?
                .peer()
                .clone()
        };

        let result = peer.call_tool(params).await;

        match result {
            Ok(result) => {
                serde_json::to_value(result).context("Failed to serialize MCP tool result")
            }
            Err(err) => match err {
                ServiceError::TransportSend(_) | ServiceError::TransportClosed => {
                    log::warn!(
                        "MCP tool '{}' on '{}' transport failed, attempting reconnect: {}",
                        tool_name,
                        self.name,
                        err,
                    );

                    *self.connected.write() = false;
                    self.service.write().take();

                    // Heal the connection for future calls (best-effort)
                    if let Err(reconnect_err) = self.connect().await {
                        log::warn!(
                            "Failed to reconnect to MCP server '{}' after transport error: {}",
                            self.name,
                            reconnect_err,
                        );
                    }

                    // Return original transport error — do not retry since the
                    // tool call may have had side effects on the server
                    Err(anyhow::Error::from(err)).with_context(|| {
                        format!(
                            "MCP tool '{}' on '{}' failed due to transport error",
                            tool_name, self.name
                        )
                    })
                }
                other @ ServiceError::McpError(_) => {
                    Err(anyhow::Error::from(other)).with_context(|| {
                        format!(
                            "MCP tool '{}' on '{}' returned application error",
                            tool_name, self.name
                        )
                    })
                }
                other => Err(anyhow::Error::from(other)).with_context(|| {
                    format!("MCP tool '{}' on '{}' returned error", tool_name, self.name)
                }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::handler::server::ServerHandler;
    use rmcp::model::{
        CallToolRequestParams, CallToolResult, Content, InitializeResult, ListToolsResult,
        ServerCapabilities, Tool,
    };
    use rmcp::service::{serve_server, NotificationContext, RoleServer};
    use serde_json::{json, Map};
    use std::time::Duration;
    use tokio::io::duplex;

    #[derive(Clone, Default, Debug)]
    struct MockServerHandler {
        initialized_params: Arc<RwLock<Option<InitializeRequestParams>>>,
        roots_list_changed_notified: Arc<RwLock<bool>>,
        peer: Arc<RwLock<Option<rmcp::service::Peer<rmcp::service::RoleServer>>>>,
        tools: Arc<RwLock<Vec<Tool>>>,
        last_tool_call: Arc<RwLock<Option<(String, Value)>>>,
    }

    impl ServerHandler for MockServerHandler {
        fn get_info(&self) -> InitializeResult {
            InitializeResult::new(ServerCapabilities::default())
                .with_server_info(Implementation::new("mock-server", "0.1.0"))
        }

        async fn initialize(
            &self,
            params: InitializeRequestParams,
            cx: RequestContext<rmcp::service::RoleServer>,
        ) -> Result<InitializeResult, rmcp::model::ErrorData> {
            *self.initialized_params.write() = Some(params);
            *self.peer.write() = Some(cx.peer.clone());
            Ok(self.get_info())
        }

        async fn list_tools(
            &self,
            _params: Option<rmcp::model::PaginatedRequestParams>,
            _cx: RequestContext<rmcp::service::RoleServer>,
        ) -> Result<ListToolsResult, rmcp::model::ErrorData> {
            Ok(ListToolsResult {
                meta: None,
                tools: self.tools.read().clone(),
                next_cursor: None,
            })
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _cx: RequestContext<RoleServer>,
        ) -> Result<CallToolResult, rmcp::model::ErrorData> {
            let arguments = request
                .arguments
                .clone()
                .map(Value::Object)
                .unwrap_or(Value::Null);
            *self.last_tool_call.write() = Some((request.name.to_string(), arguments.clone()));

            match request.name.as_ref() {
                "read" => {
                    let path = arguments
                        .get("path")
                        .and_then(Value::as_str)
                        .unwrap_or("<missing>");
                    Ok(CallToolResult::success(vec![Content::text(format!(
                        "mock contents from {path}"
                    ))]))
                }
                other => Err(rmcp::model::ErrorData::invalid_params(
                    format!("unknown tool: {other}"),
                    None,
                )),
            }
        }

        async fn on_roots_list_changed(&self, _cx: NotificationContext<rmcp::service::RoleServer>) {
            *self.roots_list_changed_notified.write() = true;
        }
    }

    fn test_mcp_config(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: "mock-mcp".to_string(),
            args: vec![],
            env: HashMap::new(),
            roots: vec![],
            enabled: true,
            description: None,
            rename_tools: HashMap::new(),
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

        let roots_result = server_peer.list_roots().await.unwrap();
        assert_eq!(roots_result.roots.len(), 1);
        let uri = &roots_result.roots[0].uri;
        assert!(
            uri.starts_with("file:///") && uri.ends_with("/test/root"),
            "expected file:///...test/root, got: {uri}"
        );

        {
            roots.write().push("/test/root2".to_string());
            client_peer.notify_roots_list_changed().await.unwrap();
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(*mock_server.roots_list_changed_notified.read());

        let roots_result = server_peer.list_roots().await.unwrap();
        assert_eq!(roots_result.roots.len(), 2);
        let uri2 = &roots_result.roots[1].uri;
        assert!(
            uri2.starts_with("file:///") && uri2.ends_with("/test/root2"),
            "expected file:///...test/root2, got: {uri2}"
        );
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
        let expected_uri = path_to_file_uri(&expected_path);
        assert_eq!(uri, expected_uri);
    }

    #[test]
    fn test_mcp_roots_expansion() {
        let mut config = test_mcp_config("test");
        std::env::set_var("TEST_ROOT", "/tmp/test");
        config.roots = vec!["$TEST_ROOT/a".to_string(), "~/b".to_string()];

        let client = McpClient::new(config);
        let roots = client.get_roots();

        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], "/tmp/test/a");
        let home = dirs::home_dir().unwrap();
        assert_eq!(roots[1], format!("{}/b", home.to_string_lossy()));
    }

    #[tokio::test]
    async fn test_mcp_roots_add_expansion() {
        let config = test_mcp_config("test");
        std::env::set_var("ADD_TEST_ROOT", "/tmp/add_test");
        let client = McpClient::new(config);

        client.add_root("$ADD_TEST_ROOT/c").await.unwrap();
        let roots = client.get_roots();

        assert!(roots.contains(&"/tmp/add_test/c".to_string()));
    }

    #[tokio::test]
    async fn test_mcp_manager_find_client_for_tool() {
        let manager = McpManager::new();
        let client = Arc::new(McpClient::new(test_mcp_config("fs")));
        *client.tools.write() = vec![
            mcp_tool_to_declaration("fs_read", "read", "Read", &json!({})).unwrap(),
            mcp_tool_to_declaration("fs_write", "write", "Write", &json!({})).unwrap(),
        ];
        manager.clients.write().insert("fs".to_string(), client);

        assert!(manager.find_client_for_tool("fs_read").is_some());
        assert!(manager.find_client_for_tool("fs_write").is_some());
        assert!(manager.find_client_for_tool("unknown_tool").is_none());
        assert!(manager.find_client_for_tool("noprefix").is_none());
    }

    #[tokio::test]
    async fn test_mcp_manager_call_tool_routes_prefixed_names() {
        let (client_transport, server_transport) = duplex(1024);
        let mock_server = MockServerHandler {
            tools: Arc::new(RwLock::new(vec![Tool::new(
                "read",
                "Read mock file contents.",
                Map::new(),
            )])),
            ..Default::default()
        };

        let server_handler = mock_server.clone();
        let client_handler = McpClientHandler::new(Arc::new(RwLock::new(vec![])));
        let (server_res, client_res) = tokio::join!(
            serve_server(server_handler, server_transport),
            rmcp::service::serve_client(client_handler, client_transport)
        );

        let _server_service = server_res.unwrap();
        let client_service = client_res.unwrap();

        let client = Arc::new(McpClient::new(test_mcp_config("fs")));
        *client.connected.write() = true;
        *client.tools.write() = vec![mcp_tool_to_declaration(
            "fs_read",
            "read",
            "Read mock file contents.",
            &json!({}),
        )
        .unwrap()];
        *client.service.write() = Some(client_service);

        let manager = McpManager::new();
        manager.clients.write().insert("fs".to_string(), client);

        let result = manager
            .call_tool("fs_read", json!({ "path": "test.txt" }))
            .await
            .unwrap();

        let result_text = result.to_string();
        assert!(result_text.contains("mock contents from test.txt"));
        assert!(!result_text.contains("Unexpected call"));

        let last_tool_call = mock_server.last_tool_call.read().clone();
        assert_eq!(
            last_tool_call,
            Some((
                "read".to_string(),
                json!({
                    "path": "test.txt"
                }),
            ))
        );
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
                        let msg = format!(
                            "MCP server '{}' failed to connect: {}. Use '.mcp connect {}' to retry.",
                            client.name(),
                            err,
                            client.name(),
                        );
                        let event = harnx_core::event::AgentEvent::Notice(
                            harnx_core::event::NoticeEvent::Warning(msg.clone()),
                        );
                        if !harnx_core::sink::emit_agent_event(event) {
                            eprintln!("Warning: {msg}");
                        }
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

    fn invalidate_all_services(&self) {
        for client in self.clients.read().values() {
            if client.is_connected() {
                client.invalidate_service();
            }
        }
    }

    pub fn get_all_tools_blocking(&self) -> Vec<ToolDeclaration> {
        if let Ok(handle) = Handle::try_current() {
            match handle.runtime_flavor() {
                RuntimeFlavor::MultiThread => {
                    tokio::task::block_in_place(|| handle.block_on(self.get_all_tools()))
                }
                _ => {
                    // On a single-threaded runtime (e.g., the ACP server),
                    // block_in_place panics. Run the async operation on a
                    // dedicated thread with its own runtime instead.
                    std::thread::scope(|s| {
                        s.spawn(|| {
                            let rt = Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .expect("create runtime for MCP tool discovery");
                            let tools = rt.block_on(self.get_all_tools());
                            self.invalidate_all_services();
                            tools
                        })
                        .join()
                        .expect("MCP tool discovery thread panicked")
                    })
                }
            }
        } else {
            match Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => {
                    let tools = runtime.block_on(self.get_all_tools());
                    self.invalidate_all_services();
                    tools
                }
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

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let (client, server_tool_name) = self
            .find_client_for_tool(name)
            .ok_or_else(|| anyhow!("Unknown MCP tool '{}'", name))?;

        if !client.is_connected() {
            client.connect().await?;
        }

        client.call_tool(&server_tool_name, arguments).await
    }

    fn find_client_for_tool(&self, name: &str) -> Option<(Arc<McpClient>, String)> {
        for client in self.clients.read().values() {
            for tool in client.tools.read().iter() {
                if tool.name == name {
                    if let Some(ref server_tool_name) = tool.mcp_tool_name {
                        return Some((client.clone(), server_tool_name.clone()));
                    }
                }
            }
        }
        None
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

#[async_trait]
impl ToolProvider for McpManager {
    fn name(&self) -> &str {
        "mcp"
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        self.find_client_for_tool(tool_name).is_some()
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
        abort: &AbortSignal,
    ) -> Result<Value, ToolError> {
        tokio::select! {
            result = McpManager::call_tool(self, tool_name, arguments) => {
                result.map_err(ToolError::Recoverable)
            }
            _ = wait_abort_signal(abort) => {
                Err(ToolError::Fatal(anyhow!("MCP tool call aborted by user")))
            }
        }
    }
}
