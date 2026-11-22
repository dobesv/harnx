use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ErrorData, InitializeRequestParams,
    ListPromptsResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RoleClient, RoleServer, RunningService};
use rmcp::Peer;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::cli::Cli;
use crate::client_handler::RemoteClientHandler;
use crate::transport::build_transport;

/// Maps service/transport errors to JSON-RPC error frames.
/// Per SEP-1303, transport and lifecycle errors legitimately remain as
/// `Err(ErrorData)` rather than passing through as domain errors.
fn proxy_error(err: rmcp::service::ServiceError) -> ErrorData {
    // Preserve the existing JSON-RPC error mapping and upstream message text.
    ErrorData::internal_error(err.to_string(), None)
}

fn tool_call_succeeded(result: &Result<CallToolResult, ErrorData>) -> bool {
    result
        .as_ref()
        .is_ok_and(|result| result.is_error != Some(true))
}

pub struct RemoteProxyServer {
    cli: Cli,
    connection: Mutex<Option<RunningService<RoleClient, RemoteClientHandler>>>,
    shutdown: CancellationToken,
}

impl RemoteProxyServer {
    pub fn new(cli: Cli) -> Self {
        Self {
            cli,
            connection: Mutex::new(None),
            shutdown: CancellationToken::new(),
        }
    }

    pub async fn shutdown_remote(&self) -> Result<(), tokio::task::JoinError> {
        // Wake the connecting caller before waiting for its lock. A stalled
        // upstream handshake must not block shutdown or permit a later reconnect.
        self.shutdown.cancel();
        let service = self.connection.lock().await.take();
        if let Some(service) = service {
            service.cancel().await?;
        }
        Ok(())
    }

    /// Discover-flow clients skip initialize, so every forwarded request uses
    /// this same single-flight connection path. Only successful attempts are kept.
    ///
    /// The upstream handshake has no deadline: a stalled upstream holds the
    /// connection lock, blocking the first request and queueing the rest until
    /// `shutdown_remote` cancels the token. Fixing that needs a timeout local
    /// to the handshake; a transport-level (reqwest) timeout would also cut off
    /// long-running tool streams.
    async fn ensure_connected(&self) -> Result<Peer<RoleClient>, ErrorData> {
        let mut connection = self.connection.lock().await;
        if self.shutdown.is_cancelled() {
            return Err(ErrorData::internal_error(
                "remote MCP proxy is shut down",
                None,
            ));
        }
        if let Some(service) = connection.as_ref() {
            return Ok(service.peer().clone());
        }

        let service = tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => {
                return Err(ErrorData::internal_error("remote MCP proxy is shut down", None));
            }
            result = self.connect() => match result {
                Ok(service) => service,
                Err(error) => {
                    log::warn!("failed to connect to remote MCP server: {}", error.message);
                    return Err(error);
                }
            },
        };
        let peer = service.peer().clone();
        *connection = Some(service);
        Ok(peer)
    }

    async fn connect(&self) -> Result<RunningService<RoleClient, RemoteClientHandler>, ErrorData> {
        let transport = build_transport(&self.cli).await.map_err(|err| {
            ErrorData::internal_error(
                format!("failed to build remote MCP transport: {err:#}"),
                None,
            )
        })?;
        rmcp::service::serve_client(RemoteClientHandler, transport)
            .await
            .map_err(|err| {
                ErrorData::internal_error(
                    format!("failed to connect to remote MCP server: {err}"),
                    None,
                )
            })
    }
}

impl ServerHandler for RemoteProxyServer {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .build(),
        );
        info.server_info =
            rmcp::model::Implementation::new("harnx-mcp-remote", env!("CARGO_PKG_VERSION"));
        info
    }

    async fn initialize(
        &self,
        _request: InitializeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::InitializeResult, ErrorData> {
        let peer = self.ensure_connected().await?;
        // Forward what the remote negotiated, but answer under this proxy's own
        // identity. `peer_info` reports the identity as optional because a
        // discovery response need not carry one; it is dropped either way, so
        // only the negotiated fields are copied across.
        let mut info = self.get_info();
        if let Some(peer_info) = peer.peer_info() {
            info.protocol_version = peer_info.protocol_version.clone();
            info.capabilities = peer_info.capabilities.clone();
            info.instructions = peer_info.instructions.clone();
            info.meta = peer_info.meta.clone();
        }
        info.server_info =
            rmcp::model::Implementation::new("harnx-mcp-remote", env!("CARGO_PKG_VERSION"));
        Ok(info)
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let peer = self.ensure_connected().await?;
        peer.list_tools(request).await.map_err(proxy_error)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.dispatch_call_tool(request, _context)
            .await
            .map(Into::into)
    }

    async fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        let peer = self.ensure_connected().await?;
        peer.list_prompts(request).await.map_err(proxy_error)
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let peer = self.ensure_connected().await?;
        peer.list_resources(request).await.map_err(proxy_error)
    }
}

impl RemoteProxyServer {
    /// The tool dispatch, which always finishes in a single step.
    ///
    /// `call_tool` must return `CallToolResponse`, whose other variants cover
    /// elicitation and long-running tasks that this server does not use.
    /// Dispatching separately keeps every arm returning a plain
    /// `CallToolResult`.
    ///
    /// Error handling follows SEP-1303 and MCP specification: remote domain errors (including `CallToolResult`
    /// with `is_error: Some(true)`) pass through as `Ok(CallToolResult)`. Only transport
    /// and lifecycle failures (connection and service errors) become `Err(ErrorData)`
    /// JSON-RPC error frames.
    async fn dispatch_call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let peer = self.ensure_connected().await?;
        let tool_name = request.name.clone();
        let started = Instant::now();
        // `peer	call_tool` returns `Result<CallToolResult, ServiceError>`. The Ok path
        // includes remote CallToolResults where `is_error==true`, which correctly
        // propagates to callers. The Err path (transport/lifecycle failures) is mapped
        // to ErrorData via proxy_error.
        let result = peer.call_tool(request).await.map_err(proxy_error);
        // mcp-remote is stdio-only; without a cached list, labels follow the upstream tool namespace.
        harnx_metrics::record_tool_call(
            &tool_name,
            tool_call_succeeded(&result),
            started.elapsed(),
        );
        result
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use clap::Parser;
    use rmcp::service::{ClientLifecycleMode, RxJsonRpcMessage, TxJsonRpcMessage};
    use rmcp::transport::Transport;
    use rmcp::ServiceExt;
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;

    fn proxy_for_url(url: &str) -> Arc<RemoteProxyServer> {
        Arc::new(RemoteProxyServer::new(Cli::parse_from([
            "harnx-mcp-remote",
            "--url",
            url,
        ])))
    }

    async fn accept_stalled_handshake(listener: &TcpListener) -> TcpStream {
        let (mut stream, _) = timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("upstream connection should start")
            .expect("accept upstream connection");
        let mut buffer = [0; 1024];
        let read = timeout(Duration::from_secs(5), stream.read(&mut buffer))
            .await
            .expect("upstream handshake should start")
            .expect("read upstream handshake");
        assert!(read > 0, "proxy should send an upstream HTTP request");
        stream
    }

    #[tokio::test]
    async fn shutdown_cancels_stalled_lazy_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = proxy_for_url(&format!("http://{}/mcp", listener.local_addr().unwrap()));
        let (client_io, server_io) = tokio::io::duplex(4096);
        let downstream = tokio::spawn({
            let proxy = proxy.clone();
            async move { proxy.serve(server_io).await.unwrap() }
        });
        let client = rmcp::service::serve_client_with_lifecycle(
            (),
            client_io,
            ClientLifecycleMode::Auto {
                preferred_versions: vec![rmcp::model::ProtocolVersion::V_2026_07_28],
                legacy_version: None,
            },
        )
        .await
        .unwrap();
        let request = tokio::spawn({
            let peer = client.peer().clone();
            async move { peer.list_tools(None).await }
        });
        // Keep this socket open and unanswered until shutdown has completed.
        let _stalled = accept_stalled_handshake(&listener).await;
        // rmcp's serve future returns only after the first non-discover request.
        let downstream = timeout(Duration::from_secs(5), downstream)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(5), proxy.shutdown_remote())
            .await
            .expect("shutdown must not wait for upstream handshake")
            .unwrap();
        let error = timeout(Duration::from_secs(5), request)
            .await
            .unwrap()
            .unwrap()
            .expect_err("in-flight request must fail on shutdown");
        assert!(error.to_string().contains("shut down"), "{error}");
        assert!(proxy.connection.lock().await.is_none());
        assert!(proxy
            .ensure_connected()
            .await
            .unwrap_err()
            .message
            .contains("shut down"));
        client.cancel().await.unwrap();
        downstream.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_lazy_connect_can_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = proxy_for_url(&format!("http://{}/mcp", listener.local_addr().unwrap()));
        let connect = tokio::spawn({
            let proxy = proxy.clone();
            async move { proxy.ensure_connected().await }
        });
        let _stalled = accept_stalled_handshake(&listener).await;
        connect.abort();
        assert!(connect.await.unwrap_err().is_cancelled());
        assert!(proxy.connection.lock().await.is_none());
        let retry = tokio::spawn({
            let proxy = proxy.clone();
            async move { proxy.ensure_connected().await }
        });
        let _second_handshake = accept_stalled_handshake(&listener).await;
        timeout(Duration::from_secs(5), proxy.shutdown_remote())
            .await
            .unwrap()
            .unwrap();
        assert!(retry.await.unwrap().is_err());
    }

    struct ControlledCloseTransport {
        closing: Option<oneshot::Sender<()>>,
        release: oneshot::Receiver<()>,
    }

    impl Transport<RoleClient> for ControlledCloseTransport {
        type Error = std::io::Error;

        fn send(
            &mut self,
            _item: TxJsonRpcMessage<RoleClient>,
        ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
            std::future::ready(Ok(()))
        }

        async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
            std::future::pending().await
        }

        async fn close(&mut self) -> Result<(), Self::Error> {
            self.closing.take().unwrap().send(()).unwrap();
            (&mut self.release).await.unwrap();
            Ok(())
        }
    }

    #[tokio::test]
    async fn shutdown_awaits_established_upstream_cancellation() {
        let proxy = proxy_for_url("http://127.0.0.1:1/mcp");
        let (closing_tx, closing_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let service = rmcp::service::serve_directly(
            RemoteClientHandler,
            ControlledCloseTransport {
                closing: Some(closing_tx),
                release: release_rx,
            },
            None,
        );
        let peer = service.peer().clone();
        *proxy.connection.lock().await = Some(service);
        let shutdown = tokio::spawn({
            let proxy = proxy.clone();
            async move { proxy.shutdown_remote().await }
        });
        timeout(Duration::from_secs(5), closing_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !shutdown.is_finished(),
            "shutdown must await transport cleanup"
        );
        assert!(proxy
            .ensure_connected()
            .await
            .unwrap_err()
            .message
            .contains("shut down"));
        release_tx.send(()).unwrap();
        timeout(Duration::from_secs(5), shutdown)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(peer.is_transport_closed());
        assert!(proxy.connection.lock().await.is_none());
        proxy.shutdown_remote().await.unwrap();
    }
}
