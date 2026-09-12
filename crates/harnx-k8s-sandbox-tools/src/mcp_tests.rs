use super::*;
use anyhow::Result;
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{ContentBlock, ErrorData, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "mcp_tests/shared_cancellation.rs"]
mod shared_cancellation;

#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
struct CounterArgs {}

#[derive(Clone)]
struct CounterServer {
    calls: Arc<AtomicUsize>,
    tool_router: ToolRouter<Self>,
}

impl CounterServer {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl CounterServer {
    #[tool(description = "Increment this MCP session's counter")]
    fn increment(
        &self,
        Parameters(CounterArgs {}): Parameters<CounterArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let count = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(CallToolResult::success(vec![ContentBlock::text(
            count.to_string(),
        )]))
    }

    #[tool(description = "Return an MCP tool error result")]
    fn error_result(
        &self,
        Parameters(CounterArgs {}): Parameters<CounterArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::error(vec![ContentBlock::text(
            "recoverable tool failure",
        )]))
    }

    #[tool(description = "Return an MCP protocol error")]
    fn protocol_error(
        &self,
        Parameters(CounterArgs {}): Parameters<CounterArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        Err(ErrorData::internal_error("protocol tool failure", None))
    }

    #[tool(description = "Wait until the request is cancelled")]
    async fn blocking(
        &self,
        Parameters(CounterArgs {}): Parameters<CounterArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(CallToolResult::success(vec![ContentBlock::text("done")]))
    }

    #[tool(description = "Complete after pre-dispatch deadline")]
    async fn slow_success(
        &self,
        Parameters(CounterArgs {}): Parameters<CounterArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let count = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(CallToolResult::success(vec![ContentBlock::text(
            count.to_string(),
        )]))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CounterServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}

fn response_text(value: &Value) -> &str {
    value["content"][0]["text"].as_str().unwrap()
}

struct TestMcp {
    caller: StreamableHttpMcpCaller,
    endpoint: String,
    shutdown: CancellationToken,
    server: tokio::task::JoinHandle<()>,
}

impl TestMcp {
    async fn start() -> Result<Self> {
        Self::start_with_config(McpCallerConfig::default()).await
    }

    async fn start_with_config(config: McpCallerConfig) -> Result<Self> {
        let shutdown = CancellationToken::new();
        let service: StreamableHttpService<CounterServer, LocalSessionManager> =
            StreamableHttpService::new(
                || Ok(CounterServer::new()),
                Default::default(),
                StreamableHttpServerConfig::default()
                    .with_json_response(true)
                    .with_cancellation_token(shutdown.child_token())
                    .disable_allowed_hosts(),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}/mcp", listener.local_addr()?);
        let server_shutdown = shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, axum::Router::new().nest_service("/mcp", service))
                .with_graceful_shutdown(async move { server_shutdown.cancelled().await })
                .await
                .unwrap();
        });
        Ok(Self {
            caller: StreamableHttpMcpCaller::with_config(config)?,
            endpoint,
            shutdown,
            server,
        })
    }

    async fn call(
        &self,
        sandbox_id: &str,
        tool: &str,
        cancel: CancellationToken,
    ) -> Result<Value, McpCallError> {
        self.caller
            .call(
                sandbox_id,
                &self.endpoint,
                tool,
                Map::new(),
                BTreeSet::new(),
                cancel,
            )
            .await
    }

    async fn increment(&self, sandbox_id: &str) -> Result<Value, McpCallError> {
        self.call(sandbox_id, "increment", CancellationToken::new())
            .await
    }

    async fn stop(self) -> Result<()> {
        self.shutdown.cancel();
        self.server.await?;
        Ok(())
    }
}

#[tokio::test]
async fn caller_reuses_one_stateful_mcp_session_per_sandbox() -> Result<()> {
    harnx_core::require_nextest();
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::set_global_recorder(recorder).expect("test process has no metrics recorder");
    let mcp = TestMcp::start().await?;

    assert_eq!(response_text(&mcp.increment("sandbox-a").await?), "1");
    assert_eq!(response_text(&mcp.increment("sandbox-a").await?), "2");
    assert_eq!(response_text(&mcp.increment("sandbox-b").await?), "1");

    let recoverable = mcp
        .call("sandbox-a", "error_result", CancellationToken::new())
        .await?;
    assert_eq!(recoverable["isError"], true);
    assert_eq!(
        recoverable["content"][0]["text"],
        "recoverable tool failure"
    );
    let sandbox_error_metrics = snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(key, _, _, value)| {
            key.key().name() == "harnx_sandbox_gateway_operation_total"
                && key
                    .key()
                    .labels()
                    .any(|label| label.key() == "outcome" && label.value() == "sandbox_error")
                && *value == DebugValue::Counter(1)
        })
        .count();
    assert_eq!(sandbox_error_metrics, 1);

    let error = mcp
        .call("sandbox-a", "protocol_error", CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind, McpCallErrorKind::Call);
    assert!(error.message.contains("protocol tool failure"));
    // A protocol failure invalidates the session, so the replacement gets
    // a fresh process registry rather than reusing uncertain state.
    assert_eq!(response_text(&mcp.increment("sandbox-a").await?), "1");

    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = mcp.call("sandbox-a", "blocking", cancel).await.unwrap_err();
    assert_eq!(error.kind, McpCallErrorKind::Cancelled);

    mcp.caller.disconnect("sandbox-a").await;
    assert_eq!(response_text(&mcp.increment("sandbox-a").await?), "1");

    mcp.stop().await?;
    Ok(())
}

#[tokio::test]
async fn empty_session_slot_remains_reserved() {
    let slot = Arc::new(SessionSlot {
        current: Mutex::new(None),
    });

    assert!(retain_session_slot(&slot));
}

async fn unavailable_call(
    config: McpCallerConfig,
    sandbox_id: &str,
    tool: &str,
) -> Result<McpCallError> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/mcp", listener.local_addr()?);
    drop(listener);
    let caller = StreamableHttpMcpCaller::with_config(config)?;
    Ok(caller
        .call(
            sandbox_id,
            &endpoint,
            tool,
            Map::new(),
            BTreeSet::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err())
}

#[tokio::test]
async fn caller_classifies_connection_establishment_failures() -> Result<()> {
    let error = unavailable_call(
        McpCallerConfig {
            max_attempts: 1,
            ..McpCallerConfig::default()
        },
        "sandbox-unavailable",
        "increment",
    )
    .await?;

    assert_eq!(error.kind, McpCallErrorKind::Connect);
    assert!(error.message.contains("connect to sandbox MCP"));
    Ok(())
}

#[tokio::test]
async fn already_cancelled_call_skips_connection_attempt() -> Result<()> {
    let caller = StreamableHttpMcpCaller::new()?;
    let cancel = CancellationToken::new();
    cancel.cancel();

    let error = caller
        .call(
            "cancelled",
            "http://127.0.0.1:1/mcp",
            "bash_exec",
            Map::new(),
            BTreeSet::new(),
            cancel,
        )
        .await
        .unwrap_err();

    assert_eq!(error.kind, McpCallErrorKind::Cancelled);
    assert_eq!(error.attempts, 0);
    assert!(caller.sessions.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn connection_retry_exhaustion_reports_exact_attempt_count() -> Result<()> {
    let error = unavailable_call(
        McpCallerConfig {
            backoff_base: Duration::ZERO,
            backoff_cap: Duration::ZERO,
            max_attempts: 2,
            ..McpCallerConfig::default()
        },
        "retry-count",
        "bash_exec",
    )
    .await?;

    assert_eq!(error.kind, McpCallErrorKind::Connect);
    assert_eq!(error.attempts, 2);
    assert!(error.message.contains("connect to sandbox MCP"));
    Ok(())
}
