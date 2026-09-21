use opentelemetry::trace::{SpanId, SpanKind, TraceId};
use rmcp::model::RequestParamsMeta;

use super::*;

#[test]
fn mcp_http_args_accept_separate_and_equals_values() {
    let separate = vec![
        "server".to_string(),
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        "3999".to_string(),
    ];
    assert_eq!(
        mcp_http_arg_value(&separate, "--host").unwrap(),
        Some("127.0.0.1".to_string())
    );
    assert_eq!(
        mcp_http_arg_value(&separate, "--port").unwrap(),
        Some("3999".to_string())
    );

    let equals = vec![
        "server".to_string(),
        "--host=0.0.0.0".to_string(),
        "--port=3001".to_string(),
    ];
    assert_eq!(
        mcp_http_arg_value(&equals, "--host").unwrap(),
        Some("0.0.0.0".to_string())
    );
    assert_eq!(
        mcp_http_arg_value(&equals, "--port").unwrap(),
        Some("3001".to_string())
    );
}

#[test]
fn resolve_mcp_mode_mutual_exclusion() {
    let err = resolve_mcp_mode(["server", "--mcp-stdio", "--mcp-http"]).unwrap_err();
    assert_eq!(
        err.to_string(),
        "choose one of --mcp-stdio or --mcp-http, not both"
    );

    assert_eq!(
        resolve_mcp_mode(["server", "--mcp-stdio"]).unwrap(),
        Some(McpMode::Stdio)
    );
    assert_eq!(
        resolve_mcp_mode(["server", "--mcp-http"]).unwrap(),
        Some(McpMode::Http)
    );
    assert_eq!(resolve_mcp_mode(["server"]).unwrap(), None);
}

#[test]
fn mcp_http_arg_requires_separate_value() {
    let args = vec!["server".to_string(), "--port".to_string()];
    let error = mcp_http_arg_value(&args, "--port").unwrap_err();
    assert_eq!(error.to_string(), "missing value for --port");
}

#[test]
fn parent_session_argument_only_uses_transport_context() {
    let mut untrusted = serde_json::json!({
        "__harnx_parent_session_id": "other-session",
        "__harnx_tool_call_id": "model-supplied-call",
    });
    add_parent_context_args(SUBAGENT_SESSION_NEW_TOOL, None, None, &mut untrusted);
    assert!(untrusted.get("__harnx_parent_session_id").is_none());
    assert!(untrusted.get("__harnx_tool_call_id").is_none());

    add_parent_context_args(
        SUBAGENT_SESSION_NEW_TOOL,
        Some("attested-session".to_string()),
        None,
        &mut untrusted,
    );
    assert_eq!(
        untrusted,
        serde_json::json!({"__harnx_parent_session_id": "attested-session"})
    );
}

#[test]
fn mcp_adapter_preserves_serialized_call_tool_results() {
    let value = serde_json::json!({
        "content": [{"type": "text", "text": "hello"}],
        "structuredContent": {"answer": 42},
        "isError": true,
        "_meta": {"private": "value"}
    });
    let result = call_tool_result_from_value(value.clone());
    assert_eq!(serde_json::to_value(result).unwrap(), value);
}

#[test]
fn mcp_adapter_wraps_raw_json_values_as_text() {
    let result = call_tool_result_from_value(serde_json::json!({"answer": 42}));
    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 1);
    assert!(serde_json::to_value(&result.content[0]).unwrap()["text"]
        .as_str()
        .unwrap()
        .contains("\"answer\": 42"));
}

fn result_with_execution_context() -> Value {
    let observation = ExecutionContextObservation::observe(
        std::path::Path::new("/workspace"),
        std::path::Path::new("/workspace"),
    );
    serde_json::json!({
        "content": [],
        "_meta": {EXECUTION_CONTEXT_NAMESPACE: observation}
    })
}

#[test]
fn mcp_adapter_strips_unrequested_execution_context() {
    let mut result = result_with_execution_context();
    finalize_execution_context_value(
        "mcp",
        "bash",
        &RequestAttestation {
            call_id: "request-1".to_string(),
            tool: "exec".to_string(),
            capabilities: Default::default(),
        },
        &mut result,
    );

    assert!(result.get("_meta").is_none());
}

#[test]
fn mcp_adapter_attests_requested_execution_context() {
    let mut result = result_with_execution_context();
    finalize_execution_context_value(
        "mcp",
        "bash",
        &RequestAttestation {
            call_id: "request-1".to_string(),
            tool: "exec".to_string(),
            capabilities: std::collections::BTreeSet::from([
                EXECUTION_CONTEXT_NAMESPACE.to_string()
            ]),
        },
        &mut result,
    );

    let provenance = &result["_meta"][EXECUTION_CONTEXT_NAMESPACE]["provenance"];
    assert_eq!(provenance["server_scope"], "mcp");
    assert_eq!(provenance["server_identity"], "bash");
    assert_eq!(provenance["tool_name"], "exec");
    assert_eq!(provenance["call_id"], "request-1");
}

const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const PARENT_SPAN_ID: &str = "00f067aa0ba902b7";
const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

fn assert_tool_exec_parent(extract_parent: impl FnOnce() -> OtelContext) {
    let spans = harnx_telemetry::collect_test_spans(|| {
        drop(tool_exec_span("test_tool", extract_parent()));
    });
    assert_eq!(spans.len(), 1);
    let span = &spans[0];
    assert_eq!(span.name, "tool_exec");
    assert_eq!(span.span_kind, SpanKind::Server);
    assert!(span.attributes.contains(&opentelemetry::KeyValue::new(
        "harnx.tool.name",
        "test_tool"
    )));
    assert_eq!(
        span.span_context.trace_id(),
        TraceId::from_hex(TRACE_ID).expect("fixed trace ID")
    );
    assert_eq!(
        span.parent_span_id,
        SpanId::from_hex(PARENT_SPAN_ID).expect("fixed parent span ID")
    );
    assert!(span.parent_span_is_remote);
}

#[test]
fn nats_tool_exec_span_continues_extracted_parent() {
    harnx_core::require_nextest();
    let mut headers = async_nats::HeaderMap::new();
    headers.insert("traceparent", TRACEPARENT);

    assert_tool_exec_parent(|| harnx_telemetry::propagate::extract_context_from_nats(&headers));
}

#[test]
fn mcp_tool_exec_span_continues_extracted_parent() {
    harnx_core::require_nextest();
    let mut params = CallToolRequestParams::new("test_tool");
    params.set_traceparent(TRACEPARENT);

    assert_tool_exec_parent(|| harnx_telemetry::propagate::extract_context_from_mcp(&params));
}

#[tokio::test]
async fn idempotency_cache_rejects_growth_past_cap() {
    harnx_core::require_nextest();
    let cache: ReplyCache = Arc::new(Mutex::new(HashMap::new()));
    for index in 0..IDEMPOTENCY_CACHE_MAX_ENTRIES {
        assert!(matches!(
            reserve_cache_entry(&cache, &format!("key-{index}")).await,
            CacheReservation::Execute(_)
        ));
    }
    assert!(matches!(
        reserve_cache_entry(&cache, "overflow").await,
        CacheReservation::Full
    ));
    assert_eq!(cache.lock().await.len(), IDEMPOTENCY_CACHE_MAX_ENTRIES);
}

#[test]
fn parent_session_id_supports_raw_session_start_tools() {
    for tool in ["session_prompt", "session_new"] {
        assert!(
            accepts_parent_session_id(tool),
            "expected support for {tool}"
        );
    }
    assert!(!accepts_parent_session_id("session_load"));
    assert!(!accepts_parent_session_id("prompt"));
    assert!(!accepts_parent_session_id("agent_session_prompt"));
}

#[test]
fn parent_context_args_include_parent_tool_call_id() {
    let mut args = serde_json::json!({ "message": "delegate" });

    add_parent_context_args(
        SUBAGENT_SESSION_PROMPT_TOOL,
        Some("parent-session".to_string()),
        Some("parent-tool-call".to_string()),
        &mut args,
    );

    assert_eq!(args["__harnx_parent_session_id"], "parent-session");
    assert_eq!(args["__harnx_tool_call_id"], "parent-tool-call");
}

struct MetricsTestToolset;

#[async_trait::async_trait]
impl Toolset for MetricsTestToolset {
    fn name(&self) -> &str {
        "metrics-test"
    }

    fn tools(&self) -> Vec<harnx_toolset::ToolSpec> {
        vec![harnx_toolset::ToolSpec {
            cancellation_guarantee: Default::default(),
            name: "known".to_owned(),
            description: "known test tool".to_owned(),
            input_schema: serde_json::json!({ "type": "object" }),
            idempotent_hint: false,
            read_only_hint: true,
            timeout_secs: None,
            meta: None,
        }]
    }

    async fn invoke(
        &self,
        _tool: &str,
        _args: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        unreachable!("metric label test does not invoke tools")
    }
}

mod adapter_wire_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rmcp::handler::client::ClientHandler;
    use rmcp::model::{ClientCapabilities, ErrorCode, InitializeRequestParams};
    use rmcp::service::{
        serve_client, serve_server, RoleClient, RoleServer, RunningService, ServiceError,
    };
    use tokio::io::duplex;

    use super::*;

    #[derive(Default)]
    struct AdapterTestToolset {
        invocations: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Toolset for AdapterTestToolset {
        fn name(&self) -> &str {
            "adapter-test"
        }

        fn tools(&self) -> Vec<harnx_toolset::ToolSpec> {
            vec![harnx_toolset::ToolSpec {
                cancellation_guarantee: Default::default(),
                name: "known".to_owned(),
                description: "known failing test tool".to_owned(),
                input_schema: serde_json::json!({ "type": "object" }),
                idempotent_hint: false,
                read_only_hint: true,
                timeout_secs: None,
                meta: None,
            }]
        }

        async fn invoke(
            &self,
            _tool: &str,
            _args: Value,
            _cancel: CancellationToken,
        ) -> Result<Value, ToolInvokeError> {
            self.invocations.fetch_add(1, Ordering::SeqCst);
            Err(ToolInvokeError::Recoverable(
                "known tool execution failed".to_owned(),
            ))
        }
    }

    #[derive(Clone, Default)]
    struct TestClientHandler;

    impl ClientHandler for TestClientHandler {
        fn get_info(&self) -> InitializeRequestParams {
            InitializeRequestParams::new(
                ClientCapabilities::builder().build(),
                Implementation::new("adapter-test-client", "0.1"),
            )
        }
    }

    type TestServerService = RunningService<RoleServer, McpToolsetAdapter>;
    type TestClientService = RunningService<RoleClient, TestClientHandler>;

    async fn setup_client_server(
        toolset: Arc<AdapterTestToolset>,
    ) -> (TestClientService, TestServerService) {
        let (client_transport, server_transport) = duplex(65_536);
        let adapter = McpToolsetAdapter { toolset };
        let server_fut = serve_server(adapter, server_transport);
        let client_fut = serve_client(TestClientHandler, client_transport);
        let (server, client) = tokio::join!(server_fut, client_fut);
        (client.unwrap(), server.unwrap())
    }

    #[tokio::test]
    async fn unknown_tool_returns_invalid_params_protocol_error() {
        let toolset = Arc::new(AdapterTestToolset::default());
        let (client, _server) = setup_client_server(toolset.clone()).await;

        let error = client
            .peer()
            .call_tool(CallToolRequestParams::new("missing"))
            .await
            .expect_err("unknown tool must return a JSON-RPC error");

        match error {
            ServiceError::McpError(error) => {
                assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
                assert_eq!(error.message, "unknown tool: missing");
            }
            other => panic!("expected MCP invalid_params error, got {other:?}"),
        }
        assert_eq!(toolset.invocations.load(Ordering::SeqCst), 0);
        client.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn known_tool_execution_failure_returns_is_error_result() {
        let toolset = Arc::new(AdapterTestToolset::default());
        let (client, _server) = setup_client_server(toolset.clone()).await;

        let result = client
            .peer()
            .call_tool(CallToolRequestParams::new("known"))
            .await
            .expect("known tool failure must remain a tool result");

        assert_eq!(result.is_error, Some(true));
        assert!(serde_json::to_string(&result.content)
            .unwrap()
            .contains("known tool execution failed"));
        assert_eq!(toolset.invocations.load(Ordering::SeqCst), 1);
        client.cancel().await.unwrap();
    }
}

#[test]
fn distinct_unknown_tools_share_one_metric_series() {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    harnx_core::require_nextest();
    let toolset = MetricsTestToolset;
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, || {
        for requested in ["attacker-tool-one", "attacker-tool-two"] {
            harnx_metrics::record_tool_call(
                metric_tool_name(&toolset, requested),
                false,
                Duration::from_millis(1),
            );
        }
    });

    let snapshot = snapshotter.snapshot().into_vec();
    assert_eq!(snapshot.len(), 2, "unknown names must share both series");
    assert!(snapshot.iter().all(|(key, _, _, _)| key
        .key()
        .labels()
        .any(|label| label.key() == "tool" && label.value() == "unknown")));
    assert!(snapshot.iter().any(|(key, _, _, value)| {
        key.key().name() == harnx_metrics::TOOL_CALLS_TOTAL && *value == DebugValue::Counter(2)
    }));
    assert!(snapshot.iter().any(|(key, _, _, value)| {
        key.key().name() == harnx_metrics::TOOL_CALL_DURATION_SECONDS
            && matches!(value, DebugValue::Histogram(samples) if samples.len() == 2)
    }));
}

fn metric_key(
    kind: metrics_util::MetricKind,
    name: &'static str,
    labels: &[(&str, &str)],
) -> metrics_util::CompositeKey {
    metrics_util::CompositeKey::new(
        kind,
        metrics::Key::from_parts(
            name,
            labels
                .iter()
                .map(|(key, value)| metrics::Label::new((*key).to_owned(), (*value).to_owned()))
                .collect::<Vec<_>>(),
        ),
    )
}

fn assert_success_and_error_tool_metric_snapshot() {
    use metrics_util::{
        debugging::{DebugValue, DebuggingRecorder},
        MetricKind,
    };

    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let ok_elapsed = Duration::from_millis(100);
    let error_elapsed = Duration::from_millis(50);
    metrics::with_local_recorder(&recorder, || {
        harnx_metrics::record_tool_call("test_tool_ok", true, ok_elapsed);
        harnx_metrics::record_tool_call("test_tool_err", false, error_elapsed);
    });

    assert_eq!(
        snapshotter.snapshot().into_vec(),
        vec![
            (
                metric_key(
                    MetricKind::Counter,
                    harnx_metrics::TOOL_CALLS_TOTAL,
                    &[("tool", "test_tool_ok"), ("status", "ok")],
                ),
                None,
                None,
                DebugValue::Counter(1),
            ),
            (
                metric_key(
                    MetricKind::Histogram,
                    harnx_metrics::TOOL_CALL_DURATION_SECONDS,
                    &[("tool", "test_tool_ok")],
                ),
                None,
                None,
                DebugValue::Histogram(vec![ok_elapsed.as_secs_f64().into()]),
            ),
            (
                metric_key(
                    MetricKind::Counter,
                    harnx_metrics::TOOL_CALLS_TOTAL,
                    &[("tool", "test_tool_err"), ("status", "error")],
                ),
                None,
                None,
                DebugValue::Counter(1),
            ),
            (
                metric_key(
                    MetricKind::Histogram,
                    harnx_metrics::TOOL_CALL_DURATION_SECONDS,
                    &[("tool", "test_tool_err")],
                ),
                None,
                None,
                DebugValue::Histogram(vec![error_elapsed.as_secs_f64().into()]),
            ),
        ]
    );
}

#[test]
fn tool_call_metrics_recorded_on_success_and_error() {
    harnx_core::require_nextest();
    assert_success_and_error_tool_metric_snapshot();
}
