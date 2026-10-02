//! Stateful transports and teardown. Cleanup belongs to the session transport,
//! not the final handler Arc: in-flight rmcp requests can retain handler Arcs.
use crate::{bootstrap::Bootstrap, connection::Connection, handler::McpHandler};
use anyhow::{Context, Result};
use futures_util::Stream;
use harnx_runtime::nats_worker::tool_reservation::ToolReservationView;
use rmcp::{
    model::*,
    service::{RequestContext, RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{
        streamable_http_server::{
            session::{
                local::{
                    LocalSessionManager, LocalSessionManagerError, SessionConfig, SessionError,
                },
                ServerSseMessage, SessionManager,
            },
            SessionId,
        },
        StreamableHttpServerConfig, StreamableHttpService, Transport,
    },
    RoleServer, ServerHandler, ServiceExt,
};
use std::{
    borrow::Cow,
    collections::HashMap,
    future::{Future, IntoFuture},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

/// One stdio connection. EOF drains calls and releases before the runtime exits.
pub async fn run_stdio(bootstrap: Arc<Bootstrap>, view: ToolReservationView) -> Result<()> {
    let handler = McpHandler::new(bootstrap.clone(), view);
    let connection = handler.connection();
    let lifecycle = Arc::new(Lifecycle::new(TaskTracker::new()));
    lifecycle.bind(connection.clone())?;
    // Keep bootstrap alive until explicit cleanup finishes, including handshake errors.
    let transport = OwnedTransport {
        inner: rmcp::transport::IntoTransport::into_transport(rmcp::transport::stdio()),
        lifecycle,
    };
    let outcome = match handler.serve(transport).await {
        Ok(service) => service
            .waiting()
            .await
            .context("run MCP stdio server")
            .map(|_| ()),
        Err(error) => Err(anyhow::anyhow!(error).context("start MCP stdio server")),
    };
    connection
        .close()
        .await
        .context("close MCP stdio connection")?;
    outcome
}

/// HTTP listener, session policy, and shutdown for one server process.
pub struct HttpOptions {
    pub listener: tokio::net::TcpListener,
    pub session_config: SessionConfig,
    pub shutdown: CancellationToken,
}

/// Serve `/mcp` until shutdown, then close every session and await cleanup.
/// Production uses SessionConfig::default() (five-minute inactivity timeout).
pub async fn run_http(
    bootstrap: Arc<Bootstrap>,
    view: ToolReservationView,
    options: HttpOptions,
) -> Result<()> {
    let HttpOptions {
        listener,
        session_config,
        shutdown,
    } = options;
    let manager = Arc::new(OwnedSessionManager::new(session_config));
    let factory_bootstrap = bootstrap.clone();
    let service = StreamableHttpService::new(
        move || {
            Ok(HttpHandler {
                handler: McpHandler::new(factory_bootstrap.clone(), view.clone()),
                initialized: AtomicBool::new(false),
            })
        },
        manager.clone(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(true)
            .with_cancellation_token(shutdown.child_token()),
    );
    let mut guard = HttpShutdownGuard {
        manager: Some(manager.clone()),
        bootstrap: bootstrap.clone(),
        shutdown: shutdown.clone(),
    };
    let app = axum::Router::new().nest_service("/mcp", service);
    tracing::info!(address = %listener.local_addr()?, "serving stateful MCP HTTP at /mcp");
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .into_future();
    tokio::pin!(server);
    let outcome = tokio::select! {
        result = &mut server => result,
        _ = shutdown.cancelled() => {
            // Cancel calls before waiting for HTTP responses to drain.
            manager.close_all().await;
            server.await
        }
    };
    shutdown.cancel();
    manager.close_all().await;
    guard.manager = None;
    // Process bootstrap (including local worker) survives connection cleanup.
    drop(bootstrap);
    outcome.context("run MCP HTTP server")
}

// Aborting the serving future still stops sessions while the runtime is alive.
struct HttpShutdownGuard {
    manager: Option<Arc<OwnedSessionManager>>,
    bootstrap: Arc<Bootstrap>,
    shutdown: CancellationToken,
}

impl Drop for HttpShutdownGuard {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let (Some(manager), Ok(runtime)) =
            (self.manager.take(), tokio::runtime::Handle::try_current())
        {
            let bootstrap = self.bootstrap.clone();
            runtime.spawn(async move {
                manager.close_all().await;
                drop(bootstrap);
            });
        }
    }
}

struct LifecycleState {
    connection: Option<Arc<Connection>>,
    stopped: bool,
}

type LifecycleRegistry = parking_lot::Mutex<HashMap<SessionId, Arc<Lifecycle>>>;

struct SessionRegistration {
    id: SessionId,
    registry: Weak<LifecycleRegistry>,
    manager: Weak<LocalSessionManager>,
}

struct Lifecycle {
    state: parking_lot::Mutex<LifecycleState>,
    cleanup: TaskTracker,
    registration: Option<SessionRegistration>,
}

impl Lifecycle {
    fn new(cleanup: TaskTracker) -> Self {
        Self {
            state: parking_lot::Mutex::new(LifecycleState {
                connection: None,
                stopped: false,
            }),
            cleanup,
            registration: None,
        }
    }

    fn bind(&self, connection: Arc<Connection>) -> Result<()> {
        let mut state = self.state.lock();
        anyhow::ensure!(!state.stopped, "MCP transport already closed");
        state.connection = Some(connection);
        Ok(())
    }

    fn stop(&self) {
        // Serialize stop through task registration. Shutdown must not observe a
        // pruned registration before its cleanup is visible to TaskTracker::wait.
        let mut state = self.state.lock();
        if state.stopped {
            return;
        }
        state.stopped = true;
        let connection = state.connection.take();
        if let Some(connection) = &connection {
            connection.cancel();
        }
        let registry = self
            .registration
            .as_ref()
            .and_then(|r| r.registry.upgrade());
        let mut registry = registry.as_ref().map(|r| r.lock());
        self.schedule_cleanup(connection);
        // Weak backreferences avoid registry -> lifecycle -> registry cycles.
        if let (Some(registry), Some(registration)) = (&mut registry, &self.registration) {
            registry.remove(&registration.id);
        }
        // Without a runtime, Connection/ToolReservationHandle Drop stops renew.
    }
    fn schedule_cleanup(&self, connection: Option<Arc<Connection>>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let session = self
            .registration
            .as_ref()
            .and_then(|r| r.manager.upgrade().map(|manager| (manager, r.id.clone())));
        self.cleanup.spawn_on(
            async move {
                // rmcp retains handles after worker expiry until close_session.
                // Remove them before potentially slow provider/reservation cleanup.
                if let Some((manager, id)) = session {
                    log_cleanup_error(
                        manager.close_session(&id).await,
                        "failed to remove terminated MCP HTTP session",
                    );
                }
                if let Some(connection) = connection {
                    log_cleanup_error(
                        connection.close().await,
                        "MCP transport cleanup failed; reservation TTL is fallback",
                    );
                }
            },
            &runtime,
        );
    }
}

fn log_cleanup_error<E: std::fmt::Display>(result: std::result::Result<(), E>, message: &str) {
    if let Err(error) = result {
        tracing::warn!(%error, "{message}");
    }
}

struct OwnedTransport<T> {
    inner: T,
    lifecycle: Arc<Lifecycle>,
}

impl<T: Transport<RoleServer>> Transport<RoleServer> for OwnedTransport<T> {
    type Error = T::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = std::result::Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item)
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        let message = self.inner.receive().await;
        if message.is_none() {
            self.lifecycle.stop();
        }
        message
    }

    async fn close(&mut self) -> std::result::Result<(), Self::Error> {
        self.lifecycle.stop();
        self.inner.close().await
    }
}

impl<T> Drop for OwnedTransport<T> {
    fn drop(&mut self) {
        self.lifecycle.stop();
    }
}

/// Thin wrapper preserves rmcp session routing/expiry. It injects lifecycle
/// ownership into initialize and cancels calls before DELETE closes the worker.
struct OwnedSessionManager {
    inner: Arc<LocalSessionManager>,
    lifecycles: Arc<LifecycleRegistry>,
    admission: tokio::sync::Mutex<()>,
    cleanup: TaskTracker,
    stopping: AtomicBool,
}

impl OwnedSessionManager {
    fn new(session_config: SessionConfig) -> Self {
        let mut inner = LocalSessionManager::default();
        inner.session_config = session_config;
        Self {
            inner: Arc::new(inner),
            lifecycles: Default::default(),
            admission: Default::default(),
            cleanup: TaskTracker::new(),
            stopping: AtomicBool::new(false),
        }
    }

    async fn close_all(&self) {
        let ids: Vec<_> = {
            let _admission = self.admission.lock().await;
            self.stopping.store(true, Ordering::Release);
            self.lifecycles.lock().keys().cloned().collect()
        };
        for id in ids {
            if let Err(error) = self.close_session(&id).await {
                tracing::warn!(%error, "failed to close MCP HTTP session");
            }
        }
        self.cleanup.close();
        self.cleanup.wait().await;
    }
}

impl SessionManager for OwnedSessionManager {
    type Error = LocalSessionManagerError;
    type Transport = OwnedTransport<<LocalSessionManager as SessionManager>::Transport>;

    async fn create_session(
        &self,
    ) -> std::result::Result<(SessionId, Self::Transport), Self::Error> {
        let _admission = self.admission.lock().await;
        if self.stopping.load(Ordering::Acquire) {
            return Err(SessionError::SessionServiceTerminated.into());
        }
        let (id, inner) = self.inner.create_session().await?;
        let mut lifecycle = Lifecycle::new(self.cleanup.clone());
        lifecycle.registration = Some(SessionRegistration {
            id: id.clone(),
            registry: Arc::downgrade(&self.lifecycles),
            manager: Arc::downgrade(&self.inner),
        });
        let lifecycle = Arc::new(lifecycle);
        self.lifecycles.lock().insert(id.clone(), lifecycle.clone());
        Ok((id, OwnedTransport { inner, lifecycle }))
    }

    async fn initialize_session(
        &self,
        id: &SessionId,
        mut message: ClientJsonRpcMessage,
    ) -> std::result::Result<ServerJsonRpcMessage, Self::Error> {
        let lifecycle = self.lifecycles.lock().get(id).cloned();
        if let Some(lifecycle) = lifecycle {
            if let ClientJsonRpcMessage::Request(request) = &mut message {
                request.request.extensions_mut().insert(lifecycle);
            }
        }
        self.inner.initialize_session(id, message).await
    }

    async fn close_session(&self, id: &SessionId) -> std::result::Result<(), Self::Error> {
        // stop prunes the registry itself; don't hold its lock while calling it.
        let lifecycle = self.lifecycles.lock().get(id).cloned();
        if let Some(lifecycle) = lifecycle {
            lifecycle.stop();
        }
        self.inner.close_session(id).await
    }

    async fn has_session(&self, id: &SessionId) -> std::result::Result<bool, Self::Error> {
        self.inner.has_session(id).await
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> std::result::Result<
        impl Stream<Item = ServerSseMessage> + Send + Sync + 'static,
        Self::Error,
    > {
        self.inner.create_stream(id, message).await
    }

    async fn accept_message(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> std::result::Result<(), Self::Error> {
        self.inner.accept_message(id, message).await
    }

    async fn create_standalone_stream(
        &self,
        id: &SessionId,
    ) -> std::result::Result<
        impl Stream<Item = ServerSseMessage> + Send + Sync + 'static,
        Self::Error,
    > {
        self.inner.create_standalone_stream(id).await
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> std::result::Result<
        impl Stream<Item = ServerSseMessage> + Send + Sync + 'static,
        Self::Error,
    > {
        self.inner.resume(id, last_event_id).await
    }
}

struct HttpHandler {
    handler: McpHandler,
    initialized: AtomicBool,
}

impl HttpHandler {
    fn ensure_initialized(&self) -> std::result::Result<(), ErrorData> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err(ErrorData::invalid_request(
                "initialize a stateful MCP HTTP session first",
                None,
            ));
        }
        Ok(())
    }
}

impl ServerHandler for HttpHandler {
    fn get_info(&self) -> ServerConfig {
        self.handler.get_info()
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[
            ProtocolVersion::V_2025_11_25,
            ProtocolVersion::V_2025_06_18,
            ProtocolVersion::V_2025_03_26,
            ProtocolVersion::V_2024_11_05,
        ])
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<InitializeResult, ErrorData> {
        // rmcp 3.5 routes 2026-07-28+ statelessly before negotiation. Refuse it:
        // a per-request backing session would break identity and continuation.
        let lifecycle = context.extensions.get::<Arc<Lifecycle>>().ok_or_else(|| {
            ErrorData::invalid_request(
                "stateful HTTP requires MCP protocol 2025-11-25 or earlier",
                None,
            )
        })?;
        lifecycle
            .bind(self.handler.connection())
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        self.initialized.store(true, Ordering::Release);
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        // Negotiated stateless requests can bypass initialize in rmcp 3.5.
        self.ensure_initialized()?;
        self.handler.list_tools(request, context).await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        self.ensure_initialized()?;
        self.handler.call_tool(request, context).await
    }
}

#[cfg(test)]
mod tests;
