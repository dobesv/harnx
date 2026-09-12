use crate::policy::{operation_metric, retry_metric, BackoffConfig, EndReason, FailureKind};
use async_trait::async_trait;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest, CustomNotification,
    ServerResult,
};
use rmcp::service::{PeerRequestOptions, RoleClient, RunningService, ServiceError};
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

const CANCEL_NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(1);

/// Deadlines and connection-retry policy for sandbox MCP calls.
#[derive(Clone, Debug)]
pub struct McpCallerConfig {
    /// Shared deadline for slot acquisition, connection, and request submission.
    pub pre_dispatch_timeout: Duration,
    /// Response wait budget; `None` keeps the waiter unbounded.
    pub response_timeout: Option<Duration>,
    /// Initial full-jitter backoff upper bound.
    pub backoff_base: Duration,
    /// Maximum full-jitter backoff upper bound.
    pub backoff_cap: Duration,
    /// Connection attempt count, including the initial attempt.
    pub max_attempts: usize,
}

impl Default for McpCallerConfig {
    fn default() -> Self {
        Self {
            pre_dispatch_timeout: Duration::from_secs(30),
            // 25 hours covers bash_exec's documented 24-hour foreground default.
            // None remains available for operators who need an unbounded response budget.
            response_timeout: Some(Duration::from_secs(25 * 60 * 60)),
            backoff_base: Duration::from_millis(250),
            backoff_cap: Duration::from_secs(10),
            max_attempts: 5,
        }
    }
}

/// Compatibility categories used when mapping MCP failures to tool invocation errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpCallErrorKind {
    /// Connection failed before dispatch.
    Connect,
    /// Remote call or protocol response failed.
    Call,
    /// Caller requested cancellation.
    Cancelled,
    /// Configured wait budget expired.
    DeadlineExceeded,
    /// MCP result could not be serialized.
    Serialization,
    /// Stateful MCP transport closed.
    TransportClosed,
}

/// MCP call failure retaining policy classification, phase, attempts, and typed cause.
#[derive(Debug)]
pub struct McpCallError {
    /// Compatibility classification used by tool-facing error mapping.
    pub kind: McpCallErrorKind,
    /// Boundary failure classification.
    pub failure_kind: FailureKind,
    /// Policy condition that ended the call.
    pub end_reason: EndReason,
    /// Phase in which the call ended.
    pub phase: &'static str,
    /// Number of boundary attempts that started.
    pub attempts: usize,
    /// Human-readable diagnostic without metric label data.
    pub message: String,
    source: Option<anyhow::Error>,
}

#[derive(Clone, Copy)]
struct McpErrorContext {
    kind: McpCallErrorKind,
    failure_kind: FailureKind,
    phase: &'static str,
    attempts: usize,
}

impl McpCallError {
    fn new(context: McpErrorContext, message: impl Into<String>) -> Self {
        let end_reason = match context.kind {
            McpCallErrorKind::Cancelled => EndReason::Cancelled,
            McpCallErrorKind::DeadlineExceeded => EndReason::DeadlineExceeded,
            _ => EndReason::Failed,
        };
        Self {
            kind: context.kind,
            failure_kind: context.failure_kind,
            end_reason,
            phase: context.phase,
            attempts: context.attempts,
            message: message.into(),
            source: None,
        }
    }

    fn caused_by(
        context: McpErrorContext,
        message: impl Into<String>,
        source: impl Into<anyhow::Error>,
    ) -> Self {
        let mut error = Self::new(context, message);
        error.source = Some(source.into());
        error
    }

    pub(crate) fn cancelled(phase: &'static str, attempts: usize) -> Self {
        Self::new(
            McpErrorContext {
                kind: McpCallErrorKind::Cancelled,
                failure_kind: FailureKind::Internal,
                phase,
                attempts,
            },
            "tool call cancelled",
        )
    }

    pub(crate) fn call(
        failure_kind: FailureKind,
        phase: &'static str,
        attempts: usize,
        message: impl Into<String>,
    ) -> Self {
        Self::new(
            McpErrorContext {
                kind: McpCallErrorKind::Call,
                failure_kind,
                phase,
                attempts,
            },
            message,
        )
    }

    fn deadline(phase: &'static str, attempts: usize, timeout: Duration) -> Self {
        Self::new(
            McpErrorContext {
                kind: McpCallErrorKind::DeadlineExceeded,
                failure_kind: FailureKind::Timeout,
                phase,
                attempts,
            },
            format!("sandbox MCP {phase} timed out after {timeout:?}"),
        )
    }
}

impl fmt::Display for McpCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}

impl std::error::Error for McpCallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source.as_ref() as &(dyn std::error::Error + 'static))
    }
}

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
    config: McpCallerConfig,
    backoff: BackoffConfig,
    // Agentgateway creates stdio targets per frontend MCP session. Pool by
    // sandbox so bash process handles survive from spawn to later wait/log calls.
    sessions: Arc<Mutex<HashMap<String, Arc<SessionSlot>>>>,
}

struct ConnectRequest<'a> {
    sandbox_id: &'a str,
    endpoint: &'a str,
    operation: &'a str,
    cancel: &'a CancellationToken,
    deadline: tokio::time::Instant,
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
        Self::with_config(McpCallerConfig::default())
    }

    pub fn with_config(config: McpCallerConfig) -> anyhow::Result<Self> {
        let backoff =
            BackoffConfig::new(config.backoff_base, config.backoff_cap, config.max_attempts);
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(config.pre_dispatch_timeout)
                .build()?,
            config,
            backoff,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    async fn session(
        &self,
        sandbox_id: &str,
        endpoint: &str,
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

        let service = self.connect(endpoint).await?;
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
    ) -> Result<Arc<RunningService<RoleClient, ()>>, McpCallError> {
        let transport = StreamableHttpClientTransport::with_client(
            self.client.clone(),
            StreamableHttpClientTransportConfig::with_uri(endpoint),
        );
        ().serve(transport).await.map(Arc::new).map_err(|error| {
            McpCallError::caused_by(
                McpErrorContext {
                    kind: McpCallErrorKind::Connect,
                    failure_kind: FailureKind::Transport,
                    phase: "connect",
                    attempts: 1,
                },
                format!("connect to sandbox MCP at {endpoint}: {error}"),
                error,
            )
        })
    }

    async fn session_with_retry(
        &self,
        request: &ConnectRequest<'_>,
    ) -> Result<Arc<RunningService<RoleClient, ()>>, McpCallError> {
        let mut attempt: usize = 1;
        loop {
            self.check_connect_cancel(request, attempt)?;
            let mut error = match self.connect_attempt(request, attempt).await {
                Ok(service) => return Ok(service),
                Err(error) => error,
            };
            error.attempts = attempt;
            if attempt >= self.backoff.max_attempts {
                error.end_reason = EndReason::AttemptsExhausted;
                operation_metric("mcp", request.operation, "retry_exhausted");
                return Err(error);
            }
            self.wait_connect_retry(request, attempt).await?;
            retry_metric("mcp", request.operation, "connect_transport");
            attempt += 1;
        }
    }

    fn check_connect_cancel(
        &self,
        request: &ConnectRequest<'_>,
        attempt: usize,
    ) -> Result<(), McpCallError> {
        if !request.cancel.is_cancelled() {
            return Ok(());
        }
        operation_metric("mcp", request.operation, "cancelled");
        Err(McpCallError::cancelled(
            "connect",
            attempt.saturating_sub(1),
        ))
    }

    async fn connect_attempt(
        &self,
        request: &ConnectRequest<'_>,
        attempt: usize,
    ) -> Result<Arc<RunningService<RoleClient, ()>>, McpCallError> {
        tokio::select! {
            biased;
            _ = request.cancel.cancelled() => {
                operation_metric("mcp", request.operation, "cancelled");
                Err(McpCallError::cancelled("connect", attempt.saturating_sub(1)))
            }
            _ = tokio::time::sleep_until(request.deadline) => {
                operation_metric("mcp", request.operation, "timeout");
                Err(McpCallError::deadline(
                    "pre_dispatch",
                    attempt.saturating_sub(1),
                    self.config.pre_dispatch_timeout,
                ))
            }
            result = self.session(request.sandbox_id, request.endpoint) => result,
        }
    }

    async fn wait_connect_retry(
        &self,
        request: &ConnectRequest<'_>,
        attempt: usize,
    ) -> Result<(), McpCallError> {
        let remaining = request
            .deadline
            .saturating_duration_since(tokio::time::Instant::now());
        let delay = self.backoff.delay(attempt - 1, remaining);
        tokio::select! {
            biased;
            _ = request.cancel.cancelled() => {
                operation_metric("mcp", request.operation, "cancelled");
                Err(McpCallError::cancelled("backoff", attempt))
            }
            _ = tokio::time::sleep_until(request.deadline) => {
                operation_metric("mcp", request.operation, "timeout");
                Err(McpCallError::deadline(
                    "pre_dispatch",
                    attempt,
                    self.config.pre_dispatch_timeout,
                ))
            }
            _ = tokio::time::sleep(delay) => Ok(()),
        }
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

    async fn stop_waiting(
        &self,
        peer: &rmcp::service::Peer<RoleClient>,
        request_id: rmcp::model::RequestId,
        call: impl std::future::Future<Output = Result<ServerResult, ServiceError>>,
        reason: &'static str,
        error: McpCallError,
    ) -> Result<ServerResult, McpCallError> {
        let notification = peer.send_notification(
            CustomNotification::new(
                "notifications/cancelled",
                Some(serde_json::json!({"requestId": request_id, "reason": reason})),
            )
            .into(),
        );
        let _ = tokio::time::timeout(CANCEL_NOTIFICATION_TIMEOUT, notification).await;
        let acknowledgement = call.await;
        if !matches!(acknowledgement, Ok(_) | Err(ServiceError::McpError(_))) {
            // Lost transport cannot confirm that the remote handler stopped.
            return std::future::pending().await;
        }
        Err(error)
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
            operation_metric("mcp", tool, "cancelled");
            return Err(McpCallError::cancelled("pre_dispatch", 0));
        }
        let pre_dispatch_deadline = tokio::time::Instant::now() + self.config.pre_dispatch_timeout;
        let connect = ConnectRequest {
            sandbox_id,
            endpoint,
            operation: tool,
            cancel: &cancel,
            deadline: pre_dispatch_deadline,
        };
        let service = self.session_with_retry(&connect).await?;

        let mut request = CallToolRequestParams::new(tool.to_string()).with_arguments(args);
        if capabilities.contains(harnx_core::execution_context::EXECUTION_CONTEXT_NAMESPACE) {
            request.meta.get_or_insert_default().insert(
                harnx_core::execution_context::EXECUTION_CONTEXT_NAMESPACE.to_string(),
                Value::Bool(true),
            );
        }
        harnx_telemetry::propagate::inject_current_into_mcp(&mut request);
        let peer = service.peer().clone();
        let submission = peer.send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(request)),
            PeerRequestOptions::no_options(),
        );
        let handle = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                operation_metric("mcp", tool, "cancelled");
                return Err(McpCallError::cancelled("dispatch", 1));
            }
            _ = tokio::time::sleep_until(pre_dispatch_deadline) => {
                operation_metric("mcp", tool, "timeout");
                return Err(McpCallError::deadline("pre_dispatch", 1, self.config.pre_dispatch_timeout));
            }
            result = submission => match result {
                Ok(handle) => handle,
                Err(error) => {
                    self.invalidate(sandbox_id, &service).await;
                    operation_metric("mcp", tool, "transport_error");
                    return Err(service_error("dispatch", tool, error));
                }
            }
        };

        let request_id = handle.id.clone();
        let call = handle.await_response();
        tokio::pin!(call);
        let response_deadline = async {
            match self.config.response_timeout {
                Some(timeout) => tokio::time::sleep(timeout).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(response_deadline);
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                operation_metric("mcp", tool, "cancelled");
                return self.stop_waiting(
                    &peer,
                    request_id,
                    call,
                    "Harnx tool call cancelled",
                    McpCallError::cancelled("response", 1),
                ).await.map(|_| unreachable!());
            }
            _ = &mut response_deadline => {
                operation_metric("mcp", tool, "timeout");
                let timeout = self.config.response_timeout.unwrap_or_default();
                return self.stop_waiting(
                    &peer,
                    request_id,
                    call,
                    "Harnx tool response deadline exceeded",
                    McpCallError::deadline("response", 1, timeout),
                ).await.map(|_| unreachable!());
            }
            result = &mut call => match result {
                Ok(response) => response,
                Err(error) => {
                    self.invalidate(sandbox_id, &service).await;
                    let outcome = if matches!(error, ServiceError::TransportClosed) {
                        "transport_error"
                    } else {
                        "permanent_error"
                    };
                    operation_metric("mcp", tool, outcome);
                    return Err(service_error("response", tool, error));
                }
            }
        };
        let result: CallToolResult = match response {
            ServerResult::CallToolResult(result) => result,
            _ => {
                self.invalidate(sandbox_id, &service).await;
                operation_metric("mcp", tool, "permanent_error");
                return Err(McpCallError::call(
                    FailureKind::Internal,
                    "response",
                    1,
                    format!("tool '{tool}' returned an unexpected MCP response"),
                ));
            }
        };
        let sandbox_error = result.is_error == Some(true);
        let value = serde_json::to_value(result).map_err(|error| {
            operation_metric("mcp", tool, "permanent_error");
            McpCallError::caused_by(
                McpErrorContext {
                    kind: McpCallErrorKind::Serialization,
                    failure_kind: FailureKind::Internal,
                    phase: "serialization",
                    attempts: 1,
                },
                format!("serialize sandbox MCP result: {error}"),
                error,
            )
        })?;
        operation_metric(
            "mcp",
            tool,
            if sandbox_error {
                "sandbox_error"
            } else {
                "success"
            },
        );
        Ok(value)
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

fn service_error(phase: &'static str, tool: &str, error: ServiceError) -> McpCallError {
    let (kind, failure_kind) = if matches!(error, ServiceError::TransportClosed) {
        (McpCallErrorKind::TransportClosed, FailureKind::Transport)
    } else {
        (McpCallErrorKind::Call, FailureKind::Permanent)
    };
    McpCallError::caused_by(
        McpErrorContext {
            kind,
            failure_kind,
            phase,
            attempts: 1,
        },
        format!("call tool '{tool}' on sandbox: {error}"),
        error,
    )
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
