use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use harnx_core::hooks::{HookEvent, HookOutcome, HookResultControl, HooksConfig};
use harnx_hooks::{dispatch_hooks_with_count_and_manager, AsyncHookManager, PersistentHookManager};
use parking_lot::RwLock;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, Content, ErrorData, Implementation,
    InitializeRequestParams, ListRootsResult, ListToolsResult, PaginatedRequestParams, Root,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleClient, RoleServer, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::{Peer, ServerHandler};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

pub struct HooksProxyConfig {
    pub hooks: HooksConfig,
    pub child_command: String,
    pub child_args: Vec<String>,
    pub session_id: String,
    pub cwd: PathBuf,
}

struct HooksProxyServerInner {
    config: HooksProxyConfig,
    child_peer: RwLock<Option<Peer<RoleClient>>>,
    child_tools: RwLock<Vec<Tool>>,
    child_service: RwLock<Option<RunningService<RoleClient, ChildClientHandler>>>,
    persistent_manager: Arc<Mutex<PersistentHookManager>>,
    async_manager: AsyncHookManager,
    roots: RwLock<Vec<Root>>,
}

#[derive(Clone)]
pub struct HooksProxyServer {
    inner: Arc<HooksProxyServerInner>,
}

struct ChildClientHandler;

impl ChildClientHandler {
    fn new(_roots: Vec<Root>) -> Self {
        Self
    }
}

impl ClientHandler for ChildClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new("harnx-mcp-hooks-proxy", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_roots(
        &self,
        _cx: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        // Return an error so the child MCP server falls back to its CLI roots
        // (e.g. --root flags) instead of replacing them with an empty list
        // (which is what the default rmcp implementation returns).
        Err(ErrorData::method_not_found::<
            rmcp::model::ListRootsRequestMethod,
        >())
    }
}

impl HooksProxyServer {
    pub fn new(config: HooksProxyConfig) -> Self {
        Self {
            inner: Arc::new(HooksProxyServerInner {
                config,
                child_peer: RwLock::new(None),
                child_tools: RwLock::new(Vec::new()),
                child_service: RwLock::new(None),
                persistent_manager: Arc::new(Mutex::new(PersistentHookManager::new())),
                async_manager: AsyncHookManager::new(),
                roots: RwLock::new(Vec::new()),
            }),
        }
    }

    async fn child_peer(&self) -> Result<Peer<RoleClient>, ErrorData> {
        self.inner
            .child_peer
            .read()
            .clone()
            .ok_or_else(|| ErrorData::internal_error("child MCP peer not initialized", None))
    }

    async fn dispatch_event(&self, event: &HookEvent) -> HookOutcome {
        dispatch_hooks_with_count_and_manager(
            event,
            &self.inner.config.hooks.entries,
            &self.inner.config.session_id,
            &self.inner.config.cwd,
            0,
            Some(&self.inner.async_manager),
            Some(&self.inner.persistent_manager),
        )
        .await
    }

    async fn call_child_tool(
        &self,
        tool_name: String,
        tool_input: Value,
    ) -> Result<CallToolResult, ErrorData> {
        let peer = self.child_peer().await?;
        let arguments = match tool_input {
            Value::Object(map) => Some(map),
            _ => Some(Map::new()),
        };
        let mut params = CallToolRequestParams::new(tool_name);
        params.arguments = arguments;
        peer.call_tool(params)
            .await
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))
    }

    fn value_to_call_tool_result(v: Value) -> CallToolResult {
        if let Some(items) = v.get("content").and_then(Value::as_array) {
            let mut content = Vec::new();
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        content.push(Content::text(text.to_string()));
                    }
                }
            }
            if !content.is_empty() {
                return CallToolResult::success(content);
            }
        }

        let text = match v {
            Value::String(s) => s,
            other => serde_json::to_string(&other).unwrap_or_else(|_| "null".to_string()),
        };
        CallToolResult::success(vec![Content::text(text)])
    }

    fn error_result(message: String) -> CallToolResult {
        CallToolResult::error(vec![Content::text(message)])
    }

    async fn initialize_child(&self) -> Result<(), ErrorData> {
        let config = &self.inner.config;
        let mut command = tokio::process::Command::new(&config.child_command);
        command.args(&config.child_args);

        let (transport, _stderr) = TokioChildProcess::builder(command)
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        let handler = ChildClientHandler::new(self.inner.roots.read().clone());
        let service = rmcp::service::serve_client(handler, transport)
            .await
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        let peer = service.peer().clone();
        let tools_result = peer
            .list_tools(None)
            .await
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        *self.inner.child_peer.write() = Some(peer);
        *self.inner.child_tools.write() = tools_result.tools;
        *self.inner.child_service.write() = Some(service);
        Ok(())
    }
}

impl ServerHandler for HooksProxyServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn initialize(
        &self,
        _request: InitializeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::InitializeResult, ErrorData> {
        self.initialize_child().await?;
        Ok(self.get_info())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: self.inner.child_tools.read().clone(),
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let tool_name: String = request.name.to_string();
        let tool_input = request.arguments.map(Value::Object).unwrap_or(Value::Null);
        let tool_use_id = uuid::Uuid::new_v4().to_string();

        let pre_event = HookEvent::PreToolUse {
            tool_name: tool_name.clone(),
            tool_input: tool_input.clone(),
            tool_use_id: tool_use_id.clone(),
        };
        let pre_outcome = self.dispatch_event(&pre_event).await;
        match pre_outcome.control {
            HookResultControl::Block { reason } => {
                return Ok(Self::error_result(reason));
            }
            HookResultControl::Ask { reason } => {
                return Ok(Self::error_result(reason.unwrap_or_default()));
            }
            HookResultControl::Continue => {}
        }

        let final_input = pre_outcome.result.mutated_tool_input.unwrap_or(tool_input);
        match self
            .call_child_tool(tool_name.clone(), final_input.clone())
            .await
        {
            Ok(result) => {
                let tool_response = serde_json::to_value(&result).unwrap_or(Value::Null);
                let post_event = HookEvent::PostToolUse {
                    tool_name,
                    tool_use_id,
                    tool_input: final_input,
                    tool_response,
                };
                let post_outcome = self.dispatch_event(&post_event).await;
                Ok(post_outcome
                    .result
                    .mutated_tool_response
                    .map(Self::value_to_call_tool_result)
                    .unwrap_or(result))
            }
            Err(err) => {
                let message = cow_to_string(err.message.clone());
                let failure_event = HookEvent::PostToolUseFailure {
                    tool_name,
                    tool_use_id,
                    tool_input: final_input,
                    error: message.clone(),
                };
                let _ = self.dispatch_event(&failure_event).await;
                Ok(Self::error_result(message))
            }
        }
    }

    async fn on_roots_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<RoleServer>,
    ) {
        if let Ok(peer) = self.child_peer().await {
            let _ = peer.notify_roots_list_changed().await;
        }
    }

    async fn set_level(
        &self,
        _request: rmcp::model::SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        Ok(())
    }
}

fn cow_to_string(value: Cow<'_, str>) -> String {
    value.into_owned()
}

#[cfg(test)]
mod tests {
    use super::HooksProxyServer;
    use serde_json::json;

    #[test]
    fn value_to_call_tool_result_with_text_content_array() {
        let result = HooksProxyServer::value_to_call_tool_result(json!({
            "content": [{"type": "text", "text": "hello"}]
        }));
        assert_eq!(result.content.len(), 1);
    }

    #[test]
    fn value_to_call_tool_result_with_plain_string() {
        let result = HooksProxyServer::value_to_call_tool_result(json!("hello"));
        assert_eq!(result.content.len(), 1);
    }

    #[test]
    fn value_to_call_tool_result_with_object_falls_back_to_json() {
        let result = HooksProxyServer::value_to_call_tool_result(json!({"ok": true}));
        assert_eq!(result.content.len(), 1);
    }

    #[test]
    fn value_to_call_tool_result_with_multi_item_content_array() {
        let result = HooksProxyServer::value_to_call_tool_result(json!({
            "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"}
            ]
        }));
        assert_eq!(result.content.len(), 2);
    }
}
