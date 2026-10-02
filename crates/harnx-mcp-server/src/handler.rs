//! Manual rmcp handler for a dynamic, connection-local catalog.
use crate::{bootstrap::Bootstrap, connection::Connection};
use harnx_core::tool::{ToolError, ToolProviderOutput};
use harnx_runtime::nats_worker::tool_reservation::ToolReservationView;
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
        Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
    RoleServer, ServerHandler,
};
use serde_json::Value;
use std::sync::Arc;

/// Create once per MCP session. Handler clones retain the same backing session.
#[derive(Clone)]
pub struct McpHandler {
    connection: Arc<Connection>,
}

impl McpHandler {
    pub fn new(bootstrap: Arc<Bootstrap>, view: ToolReservationView) -> Self {
        Self {
            connection: Arc::new(Connection::new(bootstrap, view)),
        }
    }

    /// Keep this handle for explicit transport shutdown; call close after EOF
    /// or session removal. Don't retain it in a process-wide HTTP session factory.
    pub fn connection(&self) -> Arc<Connection> {
        self.connection.clone()
    }

    pub async fn close(&self) -> anyhow::Result<()> {
        self.connection.close().await
    }
}

impl ServerHandler for McpHandler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("harnx-mcp-server", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.connection
            .list_tools()
            .await
            .map(ListToolsResult::with_all_items)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.connection
            .call_tool(
                request.name.to_string(),
                Value::Object(request.arguments.unwrap_or_default()),
                context.ct,
            )
            .await
            .map(Into::into)
    }
}

pub(crate) fn tool_result(result: Result<ToolProviderOutput, ToolError>) -> CallToolResult {
    match result {
        Ok(output) => {
            // NATS tools can return the complete MCP result envelope, including
            // media, structuredContent, annotations, and isError. Preserve it.
            if let Ok(result) = serde_json::from_value::<CallToolResult>(output.value.clone()) {
                return result;
            }
            let text = match output.value {
                Value::String(text) => text,
                value => serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
            };
            CallToolResult::success(vec![ContentBlock::text(text)])
        }
        Err(ToolError::Recoverable(error) | ToolError::Fatal(error)) => {
            CallToolResult::error(vec![ContentBlock::text(format!("{error:#}"))])
        }
    }
}

#[cfg(test)]
mod tests;
