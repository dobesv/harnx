use async_trait::async_trait;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest, CustomNotification,
    ServerResult,
};
use rmcp::service::{PeerRequestOptions, RoleClient, RunningService};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::ServiceExt;
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CANCEL_NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpCallErrorKind {
    Connect,
    Call,
    Cancelled,
}

#[derive(Debug)]
pub struct McpCallError {
    pub kind: McpCallErrorKind,
    pub message: String,
}

impl McpCallError {
    fn new(kind: McpCallErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for McpCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}

impl std::error::Error for McpCallError {}

#[async_trait]
pub trait McpCaller: Send + Sync {
    async fn call(
        &self,
        sandbox_id: &str,
        endpoint: &str,
        tool: &str,
        args: Map<String, Value>,
        capabilities: BTreeSet<String>,
        cancel: CancellationToken,
    ) -> Result<Value, McpCallError>;

    async fn disconnect(&self, _sandbox_id: &str) {}
}

#[derive(Clone)]
pub struct StreamableHttpMcpCaller {
    client: reqwest::Client,
    // Agentgateway creates stdio targets per frontend MCP session. Pool by
    // sandbox so bash process handles survive from spawn to later wait/log calls.
    sessions: Arc<Mutex<HashMap<String, Arc<SessionSlot>>>>,
}

struct SessionSlot {
    current: Mutex<Option<PooledSession>>,
}

struct PooledSession {
    endpoint: String,
    service: Arc<RunningService<RoleClient, ()>>,
}

impl PooledSession {
    fn is_live(&self) -> bool {
        !self.service.is_closed() && !self.service.peer().is_transport_closed()
    }

    fn serves(&self, endpoint: &str) -> bool {
        self.endpoint == endpoint && self.is_live()
    }
}

fn retain_session_slot(slot: &Arc<SessionSlot>) -> bool {
    slot.current.try_lock().map_or(true, |current| {
        current.as_ref().is_none_or(PooledSession::is_live)
    })
}

impl StreamableHttpMcpCaller {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    async fn session(
        &self,
        sandbox_id: &str,
        endpoint: &str,
        cancel: &CancellationToken,
    ) -> Result<Arc<RunningService<RoleClient, ()>>, McpCallError> {
        let slot = self.reserve_slot(sandbox_id).await;
        let mut current = slot.current.lock().await;
        if let Some(service) = current
            .as_ref()
            .filter(|session| session.serves(endpoint))
            .map(|session| session.service.clone())
        {
            return Ok(service);
        }
        if let Some(previous) = current.take() {
            previous.service.cancellation_token().cancel();
        }

        let service = self.connect(endpoint, cancel).await?;
        *current = Some(PooledSession {
            endpoint: endpoint.to_string(),
            service: service.clone(),
        });
        Ok(service)
    }

    async fn reserve_slot(&self, sandbox_id: &str) -> Arc<SessionSlot> {
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, slot| retain_session_slot(slot));
        sessions
            .entry(sandbox_id.to_string())
            .or_insert_with(|| {
                Arc::new(SessionSlot {
                    current: Mutex::new(None),
                })
            })
            .clone()
    }

    async fn connect(
        &self,
        endpoint: &str,
        cancel: &CancellationToken,
    ) -> Result<Arc<RunningService<RoleClient, ()>>, McpCallError> {
        let transport = StreamableHttpClientTransport::with_client(
            self.client.clone(),
            StreamableHttpClientTransportConfig::with_uri(endpoint),
        );
        let service = tokio::select! {
            service = tokio::time::timeout(CONNECT_TIMEOUT, ().serve(transport)) => match service {
                Ok(Ok(service)) => service,
                Ok(Err(error)) => return Err(McpCallError::new(
                        McpCallErrorKind::Connect,
                        format!("connect to sandbox MCP at {endpoint}: {error}"),
                    )),
                Err(_) => return Err(McpCallError::new(
                    McpCallErrorKind::Connect,
                    format!("connect to sandbox MCP at {endpoint}: timed out after {CONNECT_TIMEOUT:?}"),
                )),
            },
            _ = cancel.cancelled() => {
                return Err(McpCallError::new(McpCallErrorKind::Cancelled, "tool call cancelled"));
            }
        };
        Ok(Arc::new(service))
    }

    async fn invalidate(&self, sandbox_id: &str, failed: &Arc<RunningService<RoleClient, ()>>) {
        let slot = self.sessions.lock().await.get(sandbox_id).cloned();
        let Some(slot) = slot else {
            return;
        };
        let mut current = slot.current.lock().await;
        if current
            .as_ref()
            .is_some_and(|session| Arc::ptr_eq(&session.service, failed))
        {
            if let Some(session) = current.take() {
                session.service.cancellation_token().cancel();
            }
        }
    }
}

#[async_trait]
impl McpCaller for StreamableHttpMcpCaller {
    async fn call(
        &self,
        sandbox_id: &str,
        endpoint: &str,
        tool: &str,
        args: Map<String, Value>,
        capabilities: BTreeSet<String>,
        cancel: CancellationToken,
    ) -> Result<Value, McpCallError> {
        if cancel.is_cancelled() {
            return Err(McpCallError::new(
                McpCallErrorKind::Cancelled,
                "tool call cancelled",
            ));
        }
        let service = self.session(sandbox_id, endpoint, &cancel).await?;

        let mut request = CallToolRequestParams::new(tool.to_string()).with_arguments(args);
        if capabilities.contains(harnx_core::execution_context::EXECUTION_CONTEXT_NAMESPACE) {
            request.meta.get_or_insert_default().insert(
                harnx_core::execution_context::EXECUTION_CONTEXT_NAMESPACE.to_string(),
                Value::Bool(true),
            );
        }
        harnx_telemetry::propagate::inject_current_into_mcp(&mut request);
        let peer = service.peer().clone();
        let handle = match peer
            .send_cancellable_request(
                ClientRequest::CallToolRequest(CallToolRequest::new(request)),
                PeerRequestOptions::no_options(),
            )
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                self.invalidate(sandbox_id, &service).await;
                return Err(McpCallError::new(
                    McpCallErrorKind::Call,
                    format!("call tool '{tool}' on sandbox: {error}"),
                ));
            }
        };
        let request_id = handle.id.clone();
        let call = handle.await_response();
        tokio::pin!(call);
        let response = tokio::select! {
            result = &mut call => match result {
                Ok(response) => response,
                Err(error) => {
                    self.invalidate(sandbox_id, &service).await;
                    return Err(McpCallError::new(
                        McpCallErrorKind::Call,
                        format!("call tool '{tool}' on sandbox: {error}"),
                    ));
                }
            },
            _ = cancel.cancelled() => {
                // A typed RMCP cancellation discards the local response waiter.
                // Preserve it while sending the identical notification on wire.
                let notification = peer.send_notification(CustomNotification::new("notifications/cancelled", Some(serde_json::json!({
                    "requestId": request_id, "reason": "Harnx tool call cancelled",
                }))).into());
                let _ = tokio::time::timeout(CANCEL_NOTIFICATION_TIMEOUT, notification).await;
                let acknowledgement = call.await;
                if !matches!(acknowledgement, Ok(_) | Err(rmcp::ServiceError::McpError(_))) {
                    // A lost connection is not proof that a remote handler
                    // stopped. Its cooperative operation remains unconfirmed.
                    return std::future::pending().await;
                }
                return Err(McpCallError::new(McpCallErrorKind::Cancelled, "tool call cancelled"));
            }
        };
        let result: CallToolResult = match response {
            ServerResult::CallToolResult(result) => result,
            _ => {
                self.invalidate(sandbox_id, &service).await;
                return Err(McpCallError::new(
                    McpCallErrorKind::Call,
                    format!("tool '{tool}' returned an unexpected MCP response"),
                ));
            }
        };
        serde_json::to_value(result).map_err(|error| {
            McpCallError::new(
                McpCallErrorKind::Call,
                format!("serialize sandbox MCP result: {error}"),
            )
        })
    }

    async fn disconnect(&self, sandbox_id: &str) {
        let slot = self.sessions.lock().await.remove(sandbox_id);
        if let Some(slot) = slot {
            if let Some(session) = slot.current.lock().await.take() {
                session.service.cancellation_token().cancel();
            }
        }
    }
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
