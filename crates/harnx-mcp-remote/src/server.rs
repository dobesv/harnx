use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    InitializeRequestParams, ListPromptsResult, ListResourcesResult, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RoleClient, RoleServer, RunningService, ServiceError};
use rmcp::Peer;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::authenticated_http::SafeHttpError;
use crate::cli::Cli;
use crate::client_handler::RemoteClientHandler;
use crate::transport::{build_client_credentials_transport, build_transport, UpstreamTransport};
use rmcp::transport::streamable_http_client::StreamableHttpError;

/// Replaces untrusted upstream text in failed tool results and decoded MCP
/// service errors under client-credentials mode. Upstream text can echo the
/// presented token (including a token that was retired mid-flight by a
/// rotation), so no part of it can be forwarded without a redaction
/// guarantee we cannot give here.
const SAFE_UPSTREAM_FAILURE: &str = "remote MCP call failed; upstream error output was withheld";

/// Maps service/transport errors to JSON-RPC error frames.
/// Per SEP-1303, transport and lifecycle errors legitimately remain as
/// `Err(ErrorData)` rather than passing through as domain errors.
fn proxy_error(err: rmcp::service::ServiceError) -> ErrorData {
    // Preserve the existing JSON-RPC error mapping and upstream message text.
    ErrorData::internal_error(err.to_string(), None)
}

/// Renders the OAuth transport's positively identified safe diagnostic, or
/// `None` when the chain is not one.
///
/// `AuthenticatedHttp::safe_error` reduces every HTTP failure to its own
/// app-authored `SafeHttpError` text or to one of rmcp's static labels
/// before rmcp's worker ever sees it. But rmcp also builds transport errors
/// *after* the adapter already ran: session recovery wraps the upstream
/// JSON-RPC error in `ReinitializationFailed` (its Display embeds the
/// upstream message and data), and response parsing keeps a body preview in
/// `UnexpectedServerResponse`. Those, and any unclassified chain, carry
/// upstream-controlled text and must not be displayed.
// qual:allow(coupling, deh) reason: "The downcast is the security boundary itself: only a transport chain positively identified as the shared adapter's own error type may display, and rmcp gives no other way to recover the concrete type from DynamicTransportError's boxed source."
fn oauth_safe_transport_text(
    chain: &(dyn std::error::Error + Send + Sync + 'static),
) -> Option<String> {
    match chain.downcast_ref::<StreamableHttpError<SafeHttpError>>()? {
        // The adapter/provider's safe, status-bearing diagnostics.
        StreamableHttpError::Client(safe) => Some(safe.to_string()),
        // rmcp-owned static labels and numeric status metadata; none of it
        // is upstream-derived.
        StreamableHttpError::HttpStatus(_)
        | StreamableHttpError::SessionExpired
        | StreamableHttpError::SessionRecoveryTimeout
        | StreamableHttpError::ServerDoesNotSupportSse
        | StreamableHttpError::ServerDoesNotSupportDeleteSession
        | StreamableHttpError::UnexpectedEndOfStream
        | StreamableHttpError::ResponseBodyTooLarge { .. }
        | StreamableHttpError::TransportChannelClosed
        | StreamableHttpError::MissingSessionIdInResponse
        | StreamableHttpError::ControlRequestTimeout => Some(chain.to_string()),
        // Untrusted: upstream JSON-RPC error text (ReinitializationFailed),
        // upstream body previews (UnexpectedServerResponse), upstream header
        // values (UnexpectedContentType, AuthRequired, InsufficientScope) and
        // everything unclassified fail closed.
        _ => None,
    }
}

/// Client-credentials mode error mapping. Decoded MCP service errors keep
/// their error code with static text. Transport errors keep the shared
/// adapter's safe status-bearing diagnostics only when positively identified
/// as such; every other variant — including rmcp's session-recovery errors,
/// whose Display embeds upstream JSON-RPC message and data — is replaced
/// with the static fallback (B1).
fn oauth_proxy_error(err: rmcp::service::ServiceError) -> ErrorData {
    match err {
        ServiceError::McpError(error) => ErrorData::new(error.code, SAFE_UPSTREAM_FAILURE, None),
        ServiceError::TransportSend(transport) => {
            match oauth_safe_transport_text(transport.error.as_ref()) {
                Some(safe) => ErrorData::internal_error(safe, None),
                None => ErrorData::internal_error(SAFE_UPSTREAM_FAILURE, None),
            }
        }
        // Local lifecycle variants are rmcp-owned labels, but they are not
        // audited against upstream-controlled content; fail closed.
        other => {
            let _ = other;
            ErrorData::internal_error(SAFE_UPSTREAM_FAILURE, None)
        }
    }
}

/// Renders an initialize failure for the connect-path JSON-RPC frame under
/// client-credentials mode. The transport chain is safe only when the
/// adapter's own diagnostic can be positively identified; `ConnectionClosed`
/// carries local rmcp stage labels only. Everything else — decoded JSON-RPC
/// initialize errors (`JsonRpcError`), messages/results Debug-printed by
/// `ExpectedInitResponse`/`ExpectedInitResult`, unknown future variants —
/// gets the static fallback (B9).
fn oauth_connect_error_text(err: &rmcp::service::ClientInitializeError) -> String {
    match err {
        rmcp::service::ClientInitializeError::TransportError { error, .. } => {
            oauth_safe_transport_text(error.error.as_ref())
        }
        rmcp::service::ClientInitializeError::ConnectionClosed(_) => Some(err.to_string()),
        _ => None,
    }
    .unwrap_or_else(|| SAFE_UPSTREAM_FAILURE.to_string())
}

/// Replace an `is_error: true` tool result's content with a static safe
/// diagnostic, preserving the failure flag and result envelope. This is an
/// `Ok` service response, so it never went through `proxy_error`; successful
/// results are never touched.
fn safe_tool_error() -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(SAFE_UPSTREAM_FAILURE)])
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

    /// Whether client-credentials proxy mode is selected. This mode changes
    /// only the boundaries: the connect branch picks the auth adapter, and all
    /// untrusted upstream text is replaced with safe diagnostics.
    fn client_credentials_mode(&self) -> bool {
        self.cli.oauth_token_url.is_some()
    }

    /// Error mapping for this server's mode.
    fn proxy_error(&self, err: rmcp::service::ServiceError) -> ErrorData {
        if self.client_credentials_mode() {
            oauth_proxy_error(err)
        } else {
            proxy_error(err)
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
        let transport = self.connect_transport().await.map_err(|err| {
            ErrorData::internal_error(
                format!("failed to build remote MCP transport: {err:#}"),
                None,
            )
        })?;
        // Two connect branches: client-credentials mode serves through the
        // auth adapter transport, everything else keeps the plain proxy
        // transport and its diagnostics unchanged.
        let result = match transport {
            UpstreamTransport::Plain(transport) => {
                rmcp::service::serve_client(RemoteClientHandler, transport).await
            }
            UpstreamTransport::Authenticated(transport) => {
                rmcp::service::serve_client(RemoteClientHandler, transport).await
            }
        };
        result.map_err(|err| {
            let rendered = if self.client_credentials_mode() {
                oauth_connect_error_text(&err)
            } else {
                err.to_string()
            };
            ErrorData::internal_error(
                format!("failed to connect to remote MCP server: {rendered}"),
                None,
            )
        })
    }

    async fn connect_transport(&self) -> anyhow::Result<UpstreamTransport> {
        if self.client_credentials_mode() {
            Ok(UpstreamTransport::Authenticated(
                build_client_credentials_transport(&self.cli).await?,
            ))
        } else {
            Ok(UpstreamTransport::Plain(build_transport(&self.cli).await?))
        }
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
        peer.list_tools(request)
            .await
            .map_err(|err| self.proxy_error(err))
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
        peer.list_prompts(request)
            .await
            .map_err(|err| self.proxy_error(err))
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let peer = self.ensure_connected().await?;
        peer.list_resources(request)
            .await
            .map_err(|err| self.proxy_error(err))
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
        let result = peer
            .call_tool(request)
            .await
            .map_err(|err| self.proxy_error(err));
        // Client-credentials mode: a failed tool result is an Ok service
        // response whose content is untrusted upstream text — it can echo the
        // presented token, including one retired mid-flight by a rotation.
        // The failure flag and envelope survive; the content does not.
        let result = match result {
            Ok(upstream) if self.client_credentials_mode() && upstream.is_error == Some(true) => {
                Ok(safe_tool_error())
            }
            other => other,
        };
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
    use rmcp::model::ErrorData;
    use rmcp::service::ServiceError;
    use rmcp::service::{
        ClientInitializeError, ClientLifecycleMode, RxJsonRpcMessage, TxJsonRpcMessage,
    };
    use rmcp::transport::Transport;
    use rmcp::transport::{streamable_http_client::StreamableHttpError, DynamicTransportError};
    use rmcp::ServiceExt;
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;
    use crate::authenticated_http::SafeHttpError;

    /// Wraps a transport error the way rmcp's service worker does, so the
    /// OAuth mappers can be audited against the real chain types.
    fn transport_send<E: std::error::Error + Send + Sync + 'static>(error: E) -> ServiceError {
        ServiceError::TransportSend(DynamicTransportError::from_parts(
            "test-transport",
            std::any::TypeId::of::<()>(),
            Box::new(error),
        ))
    }

    /// B1: rmcp builds `ReinitializationFailed` after a session-recovery
    /// re-initialization whose answer is an upstream JSON-RPC error. Its
    /// Display embeds the upstream message and data, so it must fall back
    /// to the static text — while the adapter's own `SafeHttpError`
    /// diagnostics keep flowing through.
    #[test]
    fn oauth_transport_error_whitelist_fails_closed_on_untrusted_variants() {
        let sentinels = ["reinit-msg-sentinel-4101", "reinit-data-sentinel-4102"];
        let reinit = transport_send(
            StreamableHttpError::<SafeHttpError>::ReinitializationFailed(
                ErrorData::internal_error(
                    format!("rejected {}", sentinels[0]),
                    Some(serde_json::json!({ "echo": sentinels[1] })),
                ),
            ),
        );
        let frame = oauth_proxy_error(reinit);
        assert_eq!(frame.message, SAFE_UPSTREAM_FAILURE);
        for sentinel in sentinels {
            assert!(!frame.message.contains(sentinel), "{}", frame.message);
        }

        // A body preview kept by rmcp's response parsing is upstream text too.
        let preview = transport_send(
            StreamableHttpError::<SafeHttpError>::UnexpectedServerResponse(
                format!("HTTP 200 OK: {}", sentinels[0]).into(),
            ),
        );
        assert_eq!(oauth_proxy_error(preview).message, SAFE_UPSTREAM_FAILURE);

        // Attacker-controlled header values stay behind the fallback.
        let content_type = transport_send(
            StreamableHttpError::<SafeHttpError>::UnexpectedContentType(Some(sentinels[0].into())),
        );
        assert_eq!(
            oauth_proxy_error(content_type).message,
            SAFE_UPSTREAM_FAILURE
        );
    }

    /// The adapter's positively identified safe diagnostics — the provider's
    /// status-bearing token errors and rmcp's own static labels — keep
    /// flowing; removing the whitelist would break these assertions.
    #[test]
    fn oauth_transport_error_whitelist_keeps_safe_diagnostics() {
        let refresh = transport_send(StreamableHttpError::<SafeHttpError>::Client(SafeHttpError(
            "client-credentials token refresh failed (token endpoint returned HTTP 500)"
                .to_string(),
        )));
        assert!(oauth_proxy_error(refresh)
            .message
            .contains("client-credentials token refresh failed"));

        let expired = transport_send(StreamableHttpError::<SafeHttpError>::SessionExpired);
        assert!(oauth_proxy_error(expired)
            .message
            .contains("Session expired (HTTP 404)"));
    }

    /// B9: a decoded JSON-RPC error answering the initial authenticated
    /// initialize reaches the connect path's non-transport fallback, as do
    /// the Debug-printing message/result variants. Identified adapter
    /// diagnostics and local `ConnectionClosed` labels keep their text.
    #[test]
    fn oauth_connect_error_text_uses_static_fallback_for_decoded_errors() {
        let json_rpc = ClientInitializeError::JsonRpcError(ErrorData::internal_error(
            "init-msg-sentinel-4101 echoed bearer access-token-1",
            Some(serde_json::json!({ "echo": "init-data-sentinel-4102" })),
        ));
        let rendered = oauth_connect_error_text(&json_rpc);
        assert_eq!(rendered, SAFE_UPSTREAM_FAILURE);
        assert!(!rendered.contains("sentinel"));

        let transport = ClientInitializeError::TransportError {
            error: DynamicTransportError::from_parts(
                "test-transport",
                std::any::TypeId::of::<()>(),
                Box::new(StreamableHttpError::<SafeHttpError>::Client(SafeHttpError(
                    "client-credentials token refresh failed".to_string(),
                ))),
            ),
            context: "sending initialize".into(),
        };
        assert!(oauth_connect_error_text(&transport)
            .contains("client-credentials token refresh failed"));

        // An unidentified transport chain falls back instead of echoing it.
        let unknown = ClientInitializeError::TransportError {
            error: DynamicTransportError::from_parts(
                "test-transport",
                std::any::TypeId::of::<()>(),
                Box::new(
                    StreamableHttpError::<SafeHttpError>::ReinitializationFailed(
                        ErrorData::internal_error("init-msg-sentinel-4101", None),
                    ),
                ),
            ),
            context: "sending initialize".into(),
        };
        assert_eq!(
            oauth_connect_error_text(&unknown),
            SAFE_UPSTREAM_FAILURE,
            "unknown chains must not render their Display"
        );
    }

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
