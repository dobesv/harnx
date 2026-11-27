use crate::mcp::McpToolsetAdapter;
use opentelemetry::trace::{SpanId, SpanKind, TraceId};
use rmcp::model::RequestParamsMeta;

use super::*;

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
    // Passthrough holds whether or not the caller asked for a structured
    // result: a serialized `CallToolResult` is never rewrapped.
    for requested in [false, true] {
        let result = call_tool_result_from_value(value.clone(), requested);
        assert_eq!(serde_json::to_value(result).unwrap(), value);
    }
}

#[test]
fn mcp_adapter_wraps_raw_json_values_as_text() {
    let result = call_tool_result_from_value(serde_json::json!({"answer": 42}), false);
    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.structured_content, None);
    assert_eq!(result.content.len(), 1);
    assert!(serde_json::to_value(&result.content[0]).unwrap()["text"]
        .as_str()
        .unwrap()
        .contains("\"answer\": 42"));
}

#[test]
fn mcp_adapter_wraps_raw_json_in_structured_content_when_requested() {
    let value = serde_json::json!({"answer": 42});
    let result = call_tool_result_from_value(value.clone(), true);
    assert_eq!(result.structured_content, Some(value.clone()));
    assert_eq!(result.is_error, Some(false));
    // The text block carries the compact form, matching what a native
    // caller's text rendering prints.
    assert_eq!(
        serde_json::to_value(&result.content).unwrap(),
        serde_json::json!([{"type": "text", "text": value.to_string()}])
    );
}

/// Raw tool JSON is not always an object; when a structured result is
/// requested every shape is passed through. Parity with native output is
/// full for `-o json`, but Python `tools.*` matches only for objects —
/// a native array or scalar reaches Python as rendered text
/// (`python_tools::python_value`).
#[test]
fn mcp_adapter_structured_wrap_passes_non_object_json() {
    for value in [
        serde_json::json!([1, 2, 3]),
        serde_json::json!("plain text"),
        serde_json::json!(7),
        serde_json::Value::Null,
    ] {
        let requested = call_tool_result_from_value(value.clone(), true);
        assert_eq!(requested.structured_content, Some(value.clone()), "{value}");
        let unrequested = call_tool_result_from_value(value.clone(), false);
        assert_eq!(
            unrequested.structured_content, None,
            "without the request the pretty-text wrap stays: {value}"
        );
    }
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
    for tool in ["session_prompt", "session_new", "session_send"] {
        assert!(
            accepts_parent_session_id(tool),
            "expected support for {tool}"
        );
    }
    // session_wait starts a progress watcher when its process did not dispatch
    // the prompt, and that watcher labels the child's row with the parent
    // transcript's call id.
    assert!(
        accepts_parent_session_id("session_wait"),
        "session_wait needs the parent tool call id for its watcher's row"
    );
    // Observation and control tools that never start a watcher must not
    // receive transport-owned parent args.
    for tool in [
        "session_load",
        "session_cancel",
        "prompt",
        "agent_session_prompt",
    ] {
        assert!(
            !accepts_parent_session_id(tool),
            "observation tools must not accept parent context: {tool}"
        );
    }
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
        serve_client, serve_client_with_lifecycle, serve_server, ClientLifecycleMode, RoleClient,
        RoleServer, RunningService, ServiceError,
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
        toolset: Arc<dyn Toolset>,
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

    fn timeout_policy_spec(name: &str, timeout_secs: Option<u64>) -> harnx_toolset::ToolSpec {
        harnx_toolset::ToolSpec {
            cancellation_guarantee: Default::default(),
            name: name.to_owned(),
            description: String::new(),
            input_schema: serde_json::json!({ "type": "object" }),
            idempotent_hint: false,
            read_only_hint: false,
            timeout_secs,
            meta: (name == "with_template").then(|| {
                serde_json::json!({ "call_template": "Run {{name}}" })
                    .as_object()
                    .expect("meta object")
                    .clone()
            }),
        }
    }

    struct TimeoutPolicyToolset;

    #[async_trait::async_trait]
    impl Toolset for TimeoutPolicyToolset {
        fn name(&self) -> &str {
            "timeout-policy-test"
        }

        fn tools(&self) -> Vec<harnx_toolset::ToolSpec> {
            vec![
                timeout_policy_spec("without_deadline", Some(0)),
                timeout_policy_spec("bounded", Some(45)),
                timeout_policy_spec("unadvertised", None),
                timeout_policy_spec("with_template", None),
                timeout_policy_spec("stops_on_new_message", None).with_stop_on_new_message(),
                timeout_policy_spec("script_shaped", None)
                    .with_script_output(harnx_toolset::ScriptOutput::default())
                    .with_output_schema(
                        serde_json::json!({"properties": {"text": {"type": "string"}}}),
                    ),
            ]
        }

        async fn invoke(
            &self,
            _tool: &str,
            _args: Value,
            _cancel: CancellationToken,
        ) -> Result<Value, ToolInvokeError> {
            unreachable!("timeout policy test only lists tools")
        }
    }

    fn advertised_timeout(tools: &[rmcp::model::Tool], name: &str) -> Option<serde_json::Value> {
        tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("tool '{name}' advertised"))
            .meta
            .as_ref()
            .and_then(|meta| meta.0.get(harnx_toolset::TOOL_TIMEOUT_META_KEY))
            .cloned()
    }

    fn assert_timeout_advertisement(
        tools: &[rmcp::model::Tool],
        name: &str,
        expected: Option<serde_json::Value>,
    ) {
        assert_eq!(
            advertised_timeout(tools, name),
            expected,
            "timeout for {name}"
        );
    }

    /// MCP listings carry timeout metadata so bridge callers retain server policy.
    #[tokio::test]
    async fn list_tools_carries_timeout_policy_in_meta() {
        let (client, _server) = setup_client_server(Arc::new(TimeoutPolicyToolset)).await;
        let tools = client
            .peer()
            .list_tools(Default::default())
            .await
            .expect("list tools")
            .tools;

        for (name, expected) in [
            ("without_deadline", Some(serde_json::json!(0))),
            ("bounded", Some(serde_json::json!(45))),
            ("unadvertised", None),
            ("with_template", None),
        ] {
            assert_timeout_advertisement(&tools, name, expected);
        }
        let template_meta = tools
            .iter()
            .find(|tool| tool.name == "with_template")
            .and_then(|tool| tool.meta.as_ref())
            .and_then(|meta| meta.0.get("call_template"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(template_meta, Some("Run {{name}}"));
        client.cancel().await.unwrap();
    }

    /// The `_meta` value a listing advertises for one tool, if any.
    async fn listed_meta(tool_name: &str, key: &str) -> Option<serde_json::Value> {
        let (client, _server) = setup_client_server(Arc::new(TimeoutPolicyToolset)).await;
        let tools = client
            .peer()
            .list_tools(Default::default())
            .await
            .expect("list tools")
            .tools;
        let value = tools
            .iter()
            .find(|tool| tool.name == tool_name)
            .unwrap_or_else(|| panic!("tool '{tool_name}' listed"))
            .meta
            .as_ref()
            .and_then(|meta| meta.0.get(key))
            .cloned();
        client.cancel().await.unwrap();
        value
    }

    /// MCP cannot deliver the new-message signal, so its listing never claims
    /// a tool stops on one.
    #[tokio::test]
    async fn list_tools_drops_the_stop_on_new_message_opt_in() {
        let opt_in = listed_meta(
            "stops_on_new_message",
            harnx_toolset::STOP_ON_NEW_MESSAGE_META_KEY,
        )
        .await;
        assert_eq!(opt_in, None);
    }

    /// A script-shaped tool's mapping is advertised over MCP, because the
    /// request that makes the tool answer that way now travels too.
    #[tokio::test]
    async fn list_tools_keeps_the_script_output_mapping() {
        let mapping = listed_meta("script_shaped", harnx_toolset::SCRIPT_OUTPUT_META_KEY).await;
        assert!(mapping.is_some_and(|value| value.is_object()));
        let schema = listed_meta("script_shaped", harnx_toolset::OUTPUT_SCHEMA_META_KEY).await;
        assert_eq!(
            schema,
            Some(serde_json::json!({"properties": {"text": {"type": "string"}}}))
        );
    }

    /// Records whether each invocation asked for a script-shaped result.
    #[derive(Default)]
    struct StructuredResultProbe {
        seen: std::sync::Mutex<Vec<bool>>,
    }

    #[async_trait::async_trait]
    impl Toolset for StructuredResultProbe {
        fn name(&self) -> &str {
            "probe"
        }

        fn tools(&self) -> Vec<harnx_toolset::ToolSpec> {
            vec![timeout_policy_spec("probe", None)]
        }

        async fn invoke(
            &self,
            _tool: &str,
            _args: Value,
            _cancel: CancellationToken,
        ) -> Result<Value, ToolInvokeError> {
            unreachable!("the probe is invoked with a context")
        }

        async fn invoke_with_context(
            &self,
            invocation: ToolInvocation,
        ) -> Result<Value, ToolInvokeError> {
            self.seen
                .lock()
                .expect("probe log")
                .push(invocation.context.wants_structured_result());
            Ok(serde_json::json!({"content": [], "isError": false}))
        }
    }

    /// Only a `_meta` entry that is exactly `true` requests a structured result.
    #[tokio::test]
    async fn call_tool_meta_requests_a_structured_result() {
        let probe = Arc::new(StructuredResultProbe::default());
        let (client, _server) = setup_client_server(probe.clone()).await;
        let key = harnx_toolset::CAPABILITY_STRUCTURED_RESULT;
        for entry in [Some(true), None, Some(false)] {
            let mut params = CallToolRequestParams::new("probe");
            if let Some(flag) = entry {
                params
                    .meta_mut()
                    .get_or_insert_with(rmcp::model::RequestMetaObject::new)
                    .insert(key.to_owned(), Value::Bool(flag));
            }
            client.peer().call_tool(params).await.expect("probe call");
        }
        assert_eq!(*probe.seen.lock().expect("probe log"), [true, false, false]);
        client.cancel().await.unwrap();
    }

    /// Serves one tool whose result is raw business JSON, not a serialized
    /// `CallToolResult`, like every native toolset behind `run_toolset_main`.
    struct RawJsonToolset;

    #[async_trait::async_trait]
    impl Toolset for RawJsonToolset {
        fn name(&self) -> &str {
            "raw-json"
        }

        fn tools(&self) -> Vec<harnx_toolset::ToolSpec> {
            vec![timeout_policy_spec("raw", None)]
        }

        async fn invoke(
            &self,
            _tool: &str,
            _args: Value,
            _cancel: CancellationToken,
        ) -> Result<Value, ToolInvokeError> {
            Ok(serde_json::json!({"timezone": "UTC", "is_dst": false}))
        }
    }

    /// Over the real rmcp wire: a `_meta` request for a structured result
    /// wraps the toolset's raw JSON in `structuredContent` (with its compact
    /// form as text), and the same call without the `_meta` entry keeps the
    /// pretty-text envelope with no `structuredContent`.
    #[tokio::test]
    async fn call_tool_meta_wraps_raw_json_in_structured_content() {
        let (client, _server) = setup_client_server(Arc::new(RawJsonToolset)).await;
        let raw = serde_json::json!({"timezone": "UTC", "is_dst": false});

        let mut structured = CallToolRequestParams::new("raw");
        structured
            .meta_mut()
            .get_or_insert_with(rmcp::model::RequestMetaObject::new)
            .insert(
                harnx_toolset::CAPABILITY_STRUCTURED_RESULT.to_owned(),
                Value::Bool(true),
            );
        let requested = client
            .peer()
            .call_tool(structured)
            .await
            .expect("structured call");
        assert_eq!(requested.structured_content, Some(raw.clone()));
        assert_eq!(requested.is_error, Some(false));
        assert_eq!(
            serde_json::to_value(&requested.content).unwrap(),
            serde_json::json!([{"type": "text", "text": raw.to_string()}])
        );

        let unrequested = client
            .peer()
            .call_tool(CallToolRequestParams::new("raw"))
            .await
            .expect("plain call");
        assert_eq!(unrequested.structured_content, None);
        assert!(unrequested.content[0]
            .as_text()
            .expect("text block")
            .text
            .contains("\"timezone\": \"UTC\""));

        client.cancel().await.unwrap();
    }

    /// Records what a toolset observed on each invocation: the arguments,
    /// and the input responses and request state delivered beside them.
    type ObservedInvocation = (
        Value,
        std::collections::BTreeMap<String, harnx_toolset::ToolInputResponse>,
        Value,
    );

    #[derive(Default)]
    struct RecordingInputRequiredToolset {
        seen: std::sync::Mutex<Vec<ObservedInvocation>>,
    }

    #[async_trait::async_trait]
    impl Toolset for RecordingInputRequiredToolset {
        fn name(&self) -> &str {
            "input-required"
        }

        fn tools(&self) -> Vec<harnx_toolset::ToolSpec> {
            vec![harnx_toolset::ToolSpec {
                cancellation_guarantee: Default::default(),
                name: "ask".to_owned(),
                description: "challenges until answered".to_owned(),
                input_schema: serde_json::json!({ "type": "object" }),
                idempotent_hint: false,
                read_only_hint: true,
                timeout_secs: None,
                meta: None,
            }
            .with_input_required()]
        }

        async fn invoke(
            &self,
            tool: &str,
            args: Value,
            token: tokio_util::sync::CancellationToken,
        ) -> Result<Value, ToolInvokeError> {
            self.invoke_with_context(harnx_toolset::ToolInvocation {
                tool: tool.to_owned(),
                args,
                context: Default::default(),
                cancel: token,
            })
            .await
        }

        async fn invoke_with_context(
            &self,
            invocation: ToolInvocation,
        ) -> Result<Value, ToolInvokeError> {
            self.seen.lock().expect("invocation log").push((
                invocation.args.clone(),
                invocation.context.input_responses.clone(),
                invocation.context.request_state.clone(),
            ));
            if invocation.context.input_responses.is_empty() {
                return Err(ToolInvokeError::InputRequired(Box::new(
                    harnx_toolset::ToolInputRequired::confirm(
                        "ask-key",
                        "Proceed?",
                        serde_json::json!({"round": 1}),
                    ),
                )));
            }
            Ok(serde_json::json!({
                "content": [{"type": "text", "text": "adapter done"}],
                "isError": false
            }))
        }
    }

    /// Wire up the adapter over a duplex transport with the client's
    /// discover-first lifecycle, so the pair negotiates the MRTR-capable
    /// 2026-07-28 revision the way the bridge does with modern servers.
    ///
    /// The server-side future stays in its bootstrap loop until the first
    /// non-discover request arrives (rmcp keeps the connection open to either
    /// lifecycle), so it must not be awaited alongside the client handshake.
    /// Spawn it and leak the resulting service, as the in-process mock server
    /// does.
    async fn setup_client_server_modern(toolset: std::sync::Arc<dyn Toolset>) -> TestClientService {
        let (client_transport, server_transport) = duplex(65_536);
        let adapter = McpToolsetAdapter { toolset };
        tokio::spawn(async move {
            if let Ok(service) = serve_server(adapter, server_transport).await {
                std::mem::forget(service);
            }
        });
        serve_client_with_lifecycle(
            TestClientHandler,
            client_transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![rmcp::model::ProtocolVersion::LATEST],
            },
        )
        .await
        .expect("discover handshake")
    }

    /// Issue one `tools/call` without driving MRTR follow-up rounds.
    async fn call_tool_once(
        client: &TestClientService,
        params: rmcp::model::CallToolRequestParams,
    ) -> Result<rmcp::model::CallToolResponse, ServiceError> {
        client.call_tool_once(params).await
    }

    /// Modern peers (≥2026-07-28) see an MRTR `input_required` response for a
    /// challenging tool, and a re-call carries the user's answers and the
    /// echoed state beside the unchanged arguments.
    #[tokio::test]
    async fn tool_input_required_maps_to_mrtr_for_modern_peers() {
        let toolset = std::sync::Arc::new(RecordingInputRequiredToolset::default());
        let client = setup_client_server_modern(toolset.clone()).await;

        let args: serde_json::Map<String, Value> =
            serde_json::from_value(serde_json::json!({"value": "adapter"}))
                .expect("arguments are a JSON object");
        let params = rmcp::model::CallToolRequestParams::new("ask").with_arguments(args);
        let first = call_tool_once(&client, params)
            .await
            .expect("the challenge round is a protocol response, not a transport error");

        let rmcp::model::CallToolResponse::InputRequired(result) = first else {
            panic!("modern peers see the MRTR input_required variant: {first:?}");
        };
        let (key, _) = result
            .input_requests
            .as_ref()
            .and_then(|requests| requests.iter().next())
            .expect("one input request");
        assert_eq!(key, "ask-key");
        assert_eq!(result.request_state.as_deref(), Some("{\"round\":1}"));

        // Re-call with the user's answer and the echoed state.
        let mut responses = rmcp::model::InputResponses::new();
        responses.insert(
            "ask-key".to_owned(),
            serde_json::json!({
                "action": "accept",
                "_meta": {"harnx:action": "approve", "harnx:note": "go on"}
            }),
        );
        let args: serde_json::Map<String, Value> =
            serde_json::from_value(serde_json::json!({"value": "adapter"}))
                .expect("arguments are a JSON object");
        let params = rmcp::model::CallToolRequestParams::new("ask")
            .with_arguments(args)
            .with_input_responses(responses)
            .with_request_state("{\"round\":1}".to_owned());
        let second = call_tool_once(&client, params)
            .await
            .expect("the answered round completes");
        let rmcp::model::CallToolResponse::Complete(result) = second else {
            panic!("the answered round returns a completed result: {second:?}");
        };
        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            serde_json::to_value(&result.content).unwrap(),
            serde_json::json!([{"type": "text", "text": "adapter done"}])
        );

        // The toolset saw the answers and state beside unchanged arguments.
        let seen = toolset.seen.lock().expect("invocation log").clone();
        assert_eq!(seen.len(), 2, "one challenge round and one answered round");
        let (challenge_args, challenge_responses, _) = &seen[0];
        assert_eq!(challenge_args, &serde_json::json!({"value": "adapter"}));
        assert!(
            challenge_responses.is_empty(),
            "no answers on the first round"
        );
        let (rerun_args, rerun_responses, rerun_state) = &seen[1];
        assert_eq!(
            rerun_args,
            &serde_json::json!({"value": "adapter"}),
            "the re-call keeps the arguments unchanged"
        );
        let answer = rerun_responses.get("ask-key").expect("answer delivered");
        assert_eq!(answer.action, "approve");
        assert_eq!(answer.note.as_deref(), Some("go on"));
        assert_eq!(rerun_state, &serde_json::json!({"round": 1}));
        client.cancel().await.unwrap();
    }

    /// Legacy peers see the visible `INPUT_REQUIRED_COMPAT_ERROR` result, not
    /// an MRTR response they cannot render.
    #[tokio::test]
    async fn tool_input_required_degrades_to_compat_error_for_legacy_peers() {
        let toolset = std::sync::Arc::new(RecordingInputRequiredToolset::default());
        let (client, _server) = setup_client_server(toolset.clone()).await;

        let response = call_tool_once(&client, rmcp::model::CallToolRequestParams::new("ask"))
            .await
            .expect("the legacy round is a protocol response, not a transport error");
        let rmcp::model::CallToolResponse::Complete(result) = response else {
            panic!("legacy peers never see the MRTR variant: {response:?}");
        };
        assert_eq!(result.is_error, Some(true));
        assert!(
            serde_json::to_string(&result.content)
                .expect("content serializes")
                .contains(harnx_toolset::INPUT_REQUIRED_COMPAT_ERROR),
            "the failure names the cap: {:?}",
            result.content
        );

        // The tool challenged once; nobody drives a rerun for a legacy peer.
        assert_eq!(toolset.seen.lock().expect("invocation log").len(), 1);
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
