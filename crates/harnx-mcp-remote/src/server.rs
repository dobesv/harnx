use parking_lot::RwLock;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ErrorData, InitializeRequestParams, ListPromptsResult,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleClient, RoleServer, RunningService};
use rmcp::Peer;

use crate::cli::Cli;
use crate::client_handler::RemoteClientHandler;
use crate::transport::build_transport;

fn proxy_error(err: rmcp::service::ServiceError) -> ErrorData {
    // rmcp ServiceError in 2.2.0 does not expose structured remote ErrorData
    // here, so fall back to internal_error while preserving message text.
    ErrorData::internal_error(err.to_string(), None)
}

pub struct RemoteProxyServer {
    cli: Cli,
    peer: RwLock<Option<Peer<RoleClient>>>,
    client_service: RwLock<Option<RunningService<RoleClient, RemoteClientHandler>>>,
}

impl RemoteProxyServer {
    pub fn new(cli: Cli) -> Self {
        Self {
            cli,
            peer: RwLock::new(None),
            client_service: RwLock::new(None),
        }
    }

    pub async fn shutdown_remote(&self) -> Result<(), tokio::task::JoinError> {
        *self.peer.write() = None;
        let service = { self.client_service.write().take() };
        if let Some(service) = service {
            service.cancel().await?;
        }
        Ok(())
    }

    fn peer(&self) -> Result<Peer<RoleClient>, ErrorData> {
        self.peer
            .read()
            .clone()
            .ok_or_else(|| ErrorData::internal_error("remote MCP peer not initialized", None))
    }
}

impl ServerHandler for RemoteProxyServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .build(),
        );
        info.server_info = rmcp::model::Implementation::new(
            "harnx-mcp-remote",
            env!("CARGO_PKG_VERSION"),
        );
        info
    }

    async fn initialize(
        &self,
        _request: InitializeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::InitializeResult, ErrorData> {
        let transport = build_transport(&self.cli)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        let service = rmcp::service::serve_client(RemoteClientHandler, transport)
            .await
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        let peer = service.peer().clone();
        let mut info = peer
            .peer_info()
            .map(|info| (*info).clone())
            .unwrap_or_else(|| self.get_info());
        info.server_info = rmcp::model::Implementation::new(
            "harnx-mcp-remote",
            env!("CARGO_PKG_VERSION"),
        );
        *self.peer.write() = Some(peer);
        *self.client_service.write() = Some(service);
        Ok(info)
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let peer = self.peer()?;
        peer.list_tools(request).await.map_err(proxy_error)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let peer = self.peer()?;
        peer.call_tool(request).await.map_err(proxy_error)
    }

    async fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        let peer = self.peer()?;
        peer.list_prompts(request).await.map_err(proxy_error)
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let peer = self.peer()?;
        peer.list_resources(request).await.map_err(proxy_error)
    }
}
