use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use harnx_client::{
    ChatCompletionsData, ClientCallContext, Message, MessageContent, MessageRole, Model,
    OpenAICompatibleClient,
};
use harnx_core::provider_config::openai_compatible::OpenAICompatibleConfig;
use opentelemetry::trace::TraceContextExt;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::{
    trace_service_server::{TraceService, TraceServiceServer},
    ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::any_value::Value as ProtoValue;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, Span as ProtoSpan};
use prost::Message as _;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, Implementation, InitializeRequestParams,
};
use rmcp::service::RoleClient;
use rmcp::transport::async_rw::AsyncRwTransport;
use serde_json::json;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::TcpListener as StdTcpListener;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use tonic::metadata::MetadataMap;
use tonic::transport::server::TcpIncoming;
use tonic::transport::Server;
use tracing::Instrument as _;

const INPUT_TOKENS: i64 = 11;
const OUTPUT_TOKENS: i64 = 7;
const ENV_KEYS: [&str; 9] = [
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    "OTEL_EXPORTER_OTLP_PROTOCOL",
    "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
    "OTEL_TRACES_SAMPLER",
    // Timeout vars: cleared to prevent inherited long timeouts from slowing tests
    "OTEL_EXPORTER_OTLP_TIMEOUT",
    "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT",
];

type CapturedResourceSpans = Arc<Mutex<Vec<ResourceSpans>>>;
type CapturedHeaders = Arc<Mutex<Vec<HeaderMap>>>;

struct EnvGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    /// Common helper for configuring OTLP test environment.
    ///
    /// Clears all ENV_KEYS, then sets endpoint, protocol, headers, and sampler if provided.
    /// SAFETY: nextest runs each test in a separate process, so env mutations are isolated.
    fn configure_internal(
        endpoint: Option<&str>,
        protocol: Option<&str>,
        headers: Option<&str>,
    ) -> Self {
        let saved = ENV_KEYS
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();

        // Nextest runs each test in a separate process. These mutations happen before
        // the single-thread Tokio runtime is created, so no other thread can inspect
        // the process environment concurrently.
        unsafe {
            for key in ENV_KEYS {
                std::env::remove_var(key);
            }
            if let Some(endpoint) = endpoint {
                std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint);
            }
            if let Some(protocol) = protocol {
                std::env::set_var("OTEL_EXPORTER_OTLP_PROTOCOL", protocol);
            }
            if let Some(headers) = headers {
                std::env::set_var("OTEL_EXPORTER_OTLP_HEADERS", headers);
            }
            std::env::set_var("OTEL_TRACES_SAMPLER", "always_on");
        }

        Self { saved }
    }

    fn configure(endpoint: Option<&str>) -> Self {
        Self::configure_internal(endpoint, Some("http/protobuf"), None)
    }

    fn configure_for_grpc(endpoint: Option<&str>) -> Self {
        Self::configure_internal(endpoint, Some("grpc"), None)
    }

    fn configure_for_grpc_with_headers(endpoint: Option<&str>, headers: &str) -> Self {
        Self::configure_internal(endpoint, Some("grpc"), Some(headers))
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // Runtime is dropped before this guard, preserving the single-threaded
        // environment-mutation guarantee from `configure`.
        unsafe {
            for (key, value) in self.saved.drain(..) {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

struct FakeCollector {
    resource_spans: CapturedResourceSpans,
    headers: CapturedHeaders,
    task: JoinHandle<()>,
}

impl FakeCollector {
    fn spawn(listener: StdTcpListener) -> Self {
        let resource_spans = Arc::new(Mutex::new(Vec::new()));
        let headers = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/v1/traces", post(collect_traces))
            .with_state(CollectorState {
                resource_spans: Arc::clone(&resource_spans),
                headers: Arc::clone(&headers),
            });
        let listener = tokio::net::TcpListener::from_std(listener).expect("collector listener");
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("fake collector serve");
        });
        Self {
            resource_spans,
            headers,
            task,
        }
    }

    fn spans(&self) -> Vec<ProtoSpan> {
        self.resource_spans
            .lock()
            .expect("collector resource spans lock")
            .iter()
            .flat_map(|resource| &resource.scope_spans)
            .flat_map(|scope| &scope.spans)
            .cloned()
            .collect()
    }

    fn exports(&self) -> usize {
        self.headers.lock().expect("collector headers lock").len()
    }
}

impl Drop for FakeCollector {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
struct CollectorState {
    resource_spans: CapturedResourceSpans,
    headers: CapturedHeaders,
}

async fn collect_traces(
    State(state): State<CollectorState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<StatusCode, (StatusCode, String)> {
    let request = ExportTraceServiceRequest::decode(body)
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
    state
        .resource_spans
        .lock()
        .expect("collector resource spans lock")
        .extend(request.resource_spans);
    state
        .headers
        .lock()
        .expect("collector headers lock")
        .push(headers);
    Ok(StatusCode::OK)
}

// -----------------------------------------------------------------------------
// FakeGrpcCollector: gRPC mock for OTLP trace export tests
// -----------------------------------------------------------------------------

type CapturedGrpcMetadata = Arc<Mutex<Vec<MetadataMap>>>;

/// A gRPC OTLP trace collector using tonic's TraceServiceServer.
///
/// Captures all received ExportTraceServiceRequests and their incoming metadata
/// (including credential headers from OTEL_EXPORTER_OTLP_HEADERS).
struct FakeGrpcCollector {
    resource_spans: CapturedResourceSpans,
    metadata: CapturedGrpcMetadata,
    task: JoinHandle<()>,
}

#[derive(Clone)]
struct GrpcCollectorState {
    resource_spans: CapturedResourceSpans,
    metadata: CapturedGrpcMetadata,
}

#[tonic::async_trait]
impl TraceService for GrpcCollectorState {
    async fn export(
        &self,
        request: tonic::Request<ExportTraceServiceRequest>,
    ) -> Result<tonic::Response<ExportTraceServiceResponse>, tonic::Status> {
        // Capture the metadata (headers) from the request
        self.metadata
            .lock()
            .expect("grpc collector metadata lock")
            .push(request.metadata().clone());

        // Capture the resource spans from the request body
        self.resource_spans
            .lock()
            .expect("grpc collector resource spans lock")
            .extend(request.into_inner().resource_spans);

        Ok(tonic::Response::new(ExportTraceServiceResponse {
            partial_success: None,
        }))
    }
}

impl FakeGrpcCollector {
    /// Spawns a gRPC trace collector on the given listener.
    ///
    /// The listener should be bound to `127.0.0.1:0` for dynamic port allocation.
    /// Returns immediately after spawning - caller should yield to allow server to bind.
    fn spawn(listener: StdTcpListener) -> Self {
        let resource_spans = Arc::new(Mutex::new(Vec::new()));
        let metadata = Arc::new(Mutex::new(Vec::new()));
        let state = GrpcCollectorState {
            resource_spans: Arc::clone(&resource_spans),
            metadata: Arc::clone(&metadata),
        };

        let listener =
            tokio::net::TcpListener::from_std(listener).expect("grpc collector listener");
        let incoming: TcpIncoming = listener.into();
        let task = tokio::spawn(async move {
            let result = Server::builder()
                .add_service(TraceServiceServer::new(state))
                .serve_with_incoming(incoming)
                .await;
            if let Err(e) = result {
                eprintln!("FakeGrpcCollector server error: {e}");
            }
        });

        Self {
            resource_spans,
            metadata,
            task,
        }
    }

    /// Waits asynchronously for at least one export to be received.
    async fn wait_for_export(&self, timeout: Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if self.exports() > 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    /// Returns all captured spans across all export requests.
    fn spans(&self) -> Vec<ProtoSpan> {
        self.resource_spans
            .lock()
            .expect("grpc collector resource spans lock")
            .iter()
            .flat_map(|resource| &resource.scope_spans)
            .flat_map(|scope| &scope.spans)
            .cloned()
            .collect()
    }

    /// Returns the captured metadata from each export request.
    fn metadata(&self) -> Vec<MetadataMap> {
        self.metadata
            .lock()
            .expect("grpc collector metadata lock")
            .iter()
            .cloned()
            .collect()
    }

    /// Returns the number of export requests received.
    fn exports(&self) -> usize {
        self.metadata
            .lock()
            .expect("grpc collector metadata lock")
            .len()
    }
}

impl Drop for FakeGrpcCollector {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct LlmStub {
    headers: CapturedHeaders,
    task: JoinHandle<()>,
}

impl LlmStub {
    fn spawn(listener: StdTcpListener) -> Self {
        let headers = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/chat/completions", post(canned_completion))
            .with_state(Arc::clone(&headers));
        let listener = tokio::net::TcpListener::from_std(listener).expect("LLM listener");
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("LLM stub serve");
        });
        Self { headers, task }
    }

    fn traceparent(&self) -> Option<String> {
        self.headers
            .lock()
            .expect("LLM headers lock")
            .first()
            .and_then(|headers| headers.get("traceparent"))
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }
}

impl Drop for LlmStub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn canned_completion(
    State(headers): State<CapturedHeaders>,
    request_headers: HeaderMap,
) -> Json<serde_json::Value> {
    headers
        .lock()
        .expect("LLM headers lock")
        .push(request_headers);
    Json(json!({
        "id": "chatcmpl-tracing-e2e",
        "object": "chat.completion",
        "created": 1,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "canned response"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": INPUT_TOKENS,
            "completion_tokens": OUTPUT_TOKENS,
            "total_tokens": INPUT_TOKENS + OUTPUT_TOKENS
        }
    }))
}

fn bind_local() -> (StdTcpListener, String) {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind local test server");
    listener
        .set_nonblocking(true)
        .expect("make local listener nonblocking");
    let endpoint = format!("http://{}", listener.local_addr().expect("local address"));
    (listener, endpoint)
}

fn test_client(api_base: String) -> OpenAICompatibleClient {
    let config = OpenAICompatibleConfig {
        name: "trace-test".to_string(),
        api_base: Some(api_base),
        api_key: Some("local-test-key".to_string()),
        model_catalog: None,
        models: Vec::new(),
        patches: None,
        extra: None,
        system_prompt_prefix: None,
        package: None,
    };
    OpenAICompatibleClient::from_config_for_test(config, Model::new("trace-test", "test-model"))
}

fn completion_data() -> ChatCompletionsData {
    ChatCompletionsData {
        messages: vec![Message::new(
            MessageRole::User,
            MessageContent::Text("local tracing test".to_string()),
        )],
        temperature: None,
        top_p: None,
        functions: None,
        stream: false,
        attachments_dir: None,
    }
}

async fn run_agent_side_call(llm_listener: StdTcpListener, llm_endpoint: String) -> LlmStub {
    let llm = LlmStub::spawn(llm_listener);
    let client = test_client(llm_endpoint);
    let agent_turn =
        tracing::info_span!(target: "harnx_runtime", "agent_turn", otel.kind = "internal");
    let output = harnx_engine::chat_completions::chat_completions_with_data(
        &client,
        completion_data(),
        &ClientCallContext::default(),
    )
    .instrument(agent_turn)
    .await
    .expect("canned chat completion");
    assert_eq!(output.input_tokens, Some(INPUT_TOKENS as u64));
    assert_eq!(output.output_tokens, Some(OUTPUT_TOKENS as u64));
    llm
}

#[derive(Debug)]
struct PropagatedParent {
    trace_id: String,
    span_id: String,
}

fn parse_traceparent(value: &str) -> PropagatedParent {
    let mut fields = value.split('-');
    assert_eq!(fields.next(), Some("00"));
    let trace_id = fields.next().expect("traceparent trace ID").to_string();
    let span_id = fields.next().expect("traceparent span ID").to_string();
    assert!(fields.next().is_some(), "traceparent flags");
    assert!(fields.next().is_none(), "traceparent has four fields");
    PropagatedParent { trace_id, span_id }
}

fn context_ids(context: &opentelemetry::Context) -> (String, String) {
    let span = context.span();
    let span_context = span.span_context();
    (
        span_context.trace_id().to_string(),
        span_context.span_id().to_string(),
    )
}

// Contract-level proof: these functions use the same public carrier helpers as
// NATS tool calls, MCP bridge calls, tool-server extraction, and activations.
// No NATS broker or spawned process is involved in this always-run test.
fn exercise_propagation_contracts() -> Vec<PropagatedParent> {
    let mut propagated = Vec::new();

    let tool_call = tracing::info_span!(target: "harnx_engine", "tool_call", otel.kind = "client");
    let mut nats_headers = async_nats::HeaderMap::new();
    tool_call.in_scope(|| harnx_telemetry::propagate::inject_current_into_nats(&mut nats_headers));
    let nats_parent = parse_traceparent(
        nats_headers
            .get("traceparent")
            .expect("NATS traceparent")
            .as_str(),
    );
    let nats_context = harnx_telemetry::propagate::extract_context_from_nats(&nats_headers);
    assert_eq!(
        context_ids(&nats_context),
        (nats_parent.trace_id.clone(), nats_parent.span_id.clone())
    );
    let tool_exec =
        tracing::info_span!(target: "harnx_toolset_server", "tool_exec", otel.kind = "server");
    harnx_telemetry::set_span_parent(&tool_exec, nats_context);
    tool_exec.in_scope(|| {});
    drop(tool_exec);
    drop(tool_call);
    propagated.push(nats_parent);

    let tool_call = tracing::info_span!(target: "harnx_engine", "tool_call", otel.kind = "client");
    let mut params = CallToolRequestParams::new("contract_test");
    tool_call.in_scope(|| harnx_telemetry::propagate::inject_current_into_mcp(&mut params));
    // rmcp moves `_meta` from params into RequestContext before calling a server handler.
    let request_meta = params.meta.take().expect("MCP request _meta");
    let mcp_context = harnx_telemetry::propagate::extract_context_from_mcp_meta(&request_meta);
    let (trace_id, span_id) = context_ids(&mcp_context);
    let mcp_parent = PropagatedParent { trace_id, span_id };
    let tool_exec =
        tracing::info_span!(target: "harnx_toolset_server", "tool_exec", otel.kind = "server");
    harnx_telemetry::set_span_parent(&tool_exec, mcp_context);
    tool_exec.in_scope(|| {});
    drop(tool_exec);
    drop(tool_call);
    propagated.push(mcp_parent);

    let publisher =
        tracing::info_span!(target: "harnx_runtime", "agent_turn", otel.kind = "internal");
    let mut activation_headers = async_nats::HeaderMap::new();
    publisher
        .in_scope(|| harnx_telemetry::propagate::inject_current_into_nats(&mut activation_headers));
    let activation_parent = parse_traceparent(
        activation_headers
            .get("traceparent")
            .expect("activation traceparent")
            .as_str(),
    );
    let activation_context =
        harnx_telemetry::propagate::extract_context_from_nats(&activation_headers);
    assert_eq!(
        context_ids(&activation_context),
        (
            activation_parent.trace_id.clone(),
            activation_parent.span_id.clone()
        )
    );
    let consumer =
        tracing::info_span!(target: "harnx_runtime", "agent_activation", otel.kind = "consumer");
    harnx_telemetry::set_span_parent(&consumer, activation_context);
    consumer.in_scope(|| {});
    drop(consumer);
    drop(publisher);
    propagated.push(activation_parent);

    propagated
}

fn bytes_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("write hex");
    }
    output
}

fn span_attribute_i64(span: &ProtoSpan, key: &str) -> Option<i64> {
    span.attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .and_then(|attribute| attribute.value.as_ref())
        .and_then(|value| value.value.as_ref())
        .and_then(|value| match value {
            ProtoValue::IntValue(value) => Some(*value),
            _ => None,
        })
}

fn spans_by_trace(spans: &[ProtoSpan]) -> HashMap<String, Vec<&ProtoSpan>> {
    let mut grouped = HashMap::new();
    for span in spans {
        grouped
            .entry(bytes_hex(&span.trace_id))
            .or_insert_with(Vec::new)
            .push(span);
    }
    grouped
}

fn assert_exported_parent_link(
    spans: &[ProtoSpan],
    propagated: &PropagatedParent,
    parent_name: &str,
    child_name: &str,
) {
    let trace = spans_by_trace(spans)
        .remove(&propagated.trace_id)
        .expect("propagated trace exported");
    let parent = trace
        .iter()
        .find(|span| bytes_hex(&span.span_id) == propagated.span_id)
        .expect("propagating span exported");
    assert_eq!(parent.name, parent_name);
    let child = trace
        .iter()
        .find(|span| {
            span.name == child_name && bytes_hex(&span.parent_span_id) == propagated.span_id
        })
        .unwrap_or_else(|| {
            panic!(
                "continued {child_name} span missing from trace: {:?}",
                spans
                    .iter()
                    .map(|span| (
                        span.name.as_str(),
                        bytes_hex(&span.trace_id),
                        bytes_hex(&span.span_id),
                        bytes_hex(&span.parent_span_id)
                    ))
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(bytes_hex(&child.trace_id), propagated.trace_id);
}

#[test]
fn otlp_agent_trace_and_all_carrier_contracts_export_end_to_end() {
    harnx_core::require_nextest();
    let (collector_listener, collector_endpoint) = bind_local();
    let (llm_listener, llm_endpoint) = bind_local();
    let _env = EnvGuard::configure(Some(&collector_endpoint));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    runtime.block_on(async move {
        let collector = FakeCollector::spawn(collector_listener);
        let telemetry = harnx_telemetry::init_telemetry("harnx-test").expect("telemetry init");
        let llm = run_agent_side_call(llm_listener, llm_endpoint).await;
        let propagated = exercise_propagation_contracts();
        let http_traceparent = parse_traceparent(
            &llm.traceparent()
                .expect("real LLM request carried traceparent"),
        );

        telemetry.shutdown().await;
        let spans = collector.spans();
        assert!(!spans.is_empty(), "fake collector received exported spans");

        let llm_request = spans
            .iter()
            .find(|span| {
                span.name == "llm_request" && bytes_hex(&span.trace_id) == http_traceparent.trace_id
            })
            .expect("exported llm_request for HTTP traceparent");
        assert_eq!(bytes_hex(&llm_request.span_id), http_traceparent.span_id);
        assert_eq!(
            span_attribute_i64(llm_request, "gen_ai.usage.input_tokens"),
            Some(INPUT_TOKENS)
        );
        assert_eq!(
            span_attribute_i64(llm_request, "gen_ai.usage.output_tokens"),
            Some(OUTPUT_TOKENS)
        );

        let agent_turn = spans
            .iter()
            .find(|span| {
                span.name == "agent_turn"
                    && span.trace_id == llm_request.trace_id
                    && span.span_id == llm_request.parent_span_id
            })
            .expect("agent_turn is llm_request parent");
        assert!(agent_turn.parent_span_id.is_empty(), "agent_turn is root");
        assert_eq!(bytes_hex(&agent_turn.trace_id), http_traceparent.trace_id);

        assert_exported_parent_link(&spans, &propagated[0], "tool_call", "tool_exec");
        assert_exported_parent_link(&spans, &propagated[1], "tool_call", "tool_exec");
        assert_exported_parent_link(&spans, &propagated[2], "agent_turn", "agent_activation");
    });
}

#[test]
fn unset_otel_endpoint_keeps_same_agent_call_inert() {
    harnx_core::require_nextest();
    let (collector_listener, _collector_endpoint) = bind_local();
    let (llm_listener, llm_endpoint) = bind_local();
    let _env = EnvGuard::configure(None);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    runtime.block_on(async move {
        let collector = FakeCollector::spawn(collector_listener);
        let telemetry =
            harnx_telemetry::init_telemetry("harnx-test").expect("inert telemetry init");
        let llm = run_agent_side_call(llm_listener, llm_endpoint).await;
        assert!(
            llm.traceparent().is_none(),
            "inert request must not invent trace context"
        );
        telemetry.shutdown().await;
        tokio::task::yield_now().await;
        assert_eq!(collector.exports(), 0);
        assert!(collector.spans().is_empty());
    });
}

#[derive(Clone)]
struct LiveMcpClient;

impl ClientHandler for LiveMcpClient {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new("harnx-telemetry-live-test", env!("CARGO_PKG_VERSION")),
        )
    }
}

fn assert_live_mcp_trace(spans: &[ProtoSpan], propagated: &PropagatedParent) {
    assert!(
        spans.iter().any(|span| span.name == "tool_exec"),
        "spawned server exported no tool_exec: {:?}",
        spans
            .iter()
            .map(|span| (
                span.name.as_str(),
                bytes_hex(&span.trace_id),
                bytes_hex(&span.parent_span_id)
            ))
            .collect::<Vec<_>>()
    );
    assert_exported_parent_link(spans, propagated, "tool_call", "tool_exec");
}

fn live_mcp_binary() -> Option<std::ffi::OsString> {
    let Some(binary) = std::env::var_os("HARNX_FS_TOOLS_BIN") else {
        eprintln!("skipping: HARNX_FS_TOOLS_BIN is not set");
        return None;
    };
    if !std::path::Path::new(&binary).is_file() {
        eprintln!("skipping: HARNX_FS_TOOLS_BIN does not name a file");
        return None;
    }
    Some(binary)
}

// =============================================================================
// gRPC e2e tests
// =============================================================================

#[test]
fn grpc_happy_path_exports_spans_to_fake_collector() {
    harnx_core::require_nextest();
    let (collector_listener, collector_endpoint) = bind_local();
    let _env = EnvGuard::configure_for_grpc(Some(&collector_endpoint));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("grpc test runtime");

    runtime.block_on(async move {
        let collector = FakeGrpcCollector::spawn(collector_listener);

        // Brief delay to ensure the gRPC server is fully bound before initializing telemetry.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let telemetry =
            harnx_telemetry::init_telemetry("grpc-happy-path-test").expect("telemetry init");

        // Emit a span with target matching the filter "harnx=info" (from DEFAULT_FILTER)
        let span = tracing::info_span!(target: "harnx_telemetry", "grpc_test_span");
        let _enter = span.enter();
        drop(_enter);
        drop(span);

        telemetry.shutdown().await;

        // Allow time for the collector to receive the export (shutdown flushes batch)
        assert!(
            collector.wait_for_export(Duration::from_secs(2)).await,
            "grpc collector received export"
        );

        let spans = collector.spans();
        assert!(!spans.is_empty(), "grpc collector received exported spans");
        assert!(
            spans.iter().any(|s| s.name == "grpc_test_span"),
            "grpc collector received the test span"
        );
    });
}

#[test]
fn grpc_credential_metadata_forwarded_on_loopback() {
    harnx_core::require_nextest();
    let (collector_listener, collector_endpoint) = bind_local();
    let _env = EnvGuard::configure_for_grpc_with_headers(
        Some(&collector_endpoint),
        "api-key=test-secret-value",
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("grpc credentials test runtime");

    runtime.block_on(async move {
        let collector = FakeGrpcCollector::spawn(collector_listener);

        // Brief delay to ensure the gRPC server is fully bound before initializing telemetry.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let telemetry =
            harnx_telemetry::init_telemetry("grpc-credentials-test").expect("telemetry init");

        // Emit a span with target matching the filter "harnx=info" (from DEFAULT_FILTER)
        let span = tracing::info_span!(target: "harnx_telemetry", "grpc_cred_test_span");
        let _enter = span.enter();
        drop(_enter);
        drop(span);

        telemetry.shutdown().await;

        // Allow time for the collector to receive the export
        assert!(
            collector.wait_for_export(Duration::from_secs(2)).await,
            "grpc collector received export"
        );

        let metadata = collector.metadata();
        assert!(!metadata.is_empty(), "grpc collector received metadata");

        // On loopback, credentials should NOT be stripped
        let first_metadata = &metadata[0];
        let api_key = first_metadata
            .get("api-key")
            .expect("api-key header present");

        // gRPC metadata values are Base64-encoded for non-ASCII, but "test-secret-value" is ASCII
        // and should come through as-is (or as a single-value string)
        let api_key_str = api_key
            .to_str()
            .expect("api-key header value is valid UTF-8");
        assert_eq!(
            api_key_str, "test-secret-value",
            "credential header preserved on loopback endpoint"
        );
    });
}

/// Windows DNS does not resolve RFC 6761 *.localhost subdomains without local hosts file mapping.
/// The test would time out on Windows CI because `collector.localhost` does not resolve.
/// Unix systems implement RFC 6761 properly, resolving *.localhost to 127.0.0.1.
#[cfg(not(windows))]
#[test]
fn grpc_credential_metadata_stripped_on_cleartext_non_loopback() {
    harnx_core::require_nextest();
    let (collector_listener, collector_endpoint) = bind_local();
    // Get dynamic port and extract it from the endpoint string
    let port = collector_endpoint
        .rsplit(':')
        .next()
        .expect("endpoint has port");
    // Use collector.localhost which resolves to 127.0.0.1 (RFC 6761), but is NOT
    // recognized as loopback by should_send_headers (which checks for literal "localhost"
    // or loopback IpAddr). This triggers the credential-stripping path.
    // Design: ALL headers in OTEL_EXPORTER_OTLP_HEADERS are treated as credential headers
    // and stripped on cleartext non-loopback endpoints.
    let endpoint = format!("http://collector.localhost:{}", port);
    let headers_env = "api-key=secret-value,authorization=token";

    let _env = EnvGuard::configure_for_grpc_with_headers(Some(&endpoint), headers_env);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("grpc credentials test runtime");

    runtime.block_on(async move {
        let collector = FakeGrpcCollector::spawn(collector_listener);

        // Brief delay to ensure the gRPC server is fully bound before initializing telemetry.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let telemetry =
            harnx_telemetry::init_telemetry("grpc-cred-strip-test").expect("telemetry init");

        // Emit a span with target matching the filter "harnx=info" (from DEFAULT_FILTER)
        let span = tracing::info_span!(target: "harnx_telemetry", "grpc_cred_strip_test_span");
        let _enter = span.enter();
        drop(_enter);
        drop(span);

        telemetry.shutdown().await;

        // Allow time for the collector to receive the export
        assert!(
            collector.wait_for_export(Duration::from_secs(2)).await,
            "grpc collector received export"
        );

        let metadata = collector.metadata();
        assert!(!metadata.is_empty(), "grpc collector received metadata");

        // On cleartext non-loopback, credential headers should be stripped.
        // All headers in OTEL_EXPORTER_OTLP_HEADERS are treated as credentials and stripped
        // on cleartext non-loopback endpoints (no mechanism to pass non-credential headers).
        let first_metadata = &metadata[0];

        // Target host is "collector.localhost" which is NOT a loopback IP or literal "localhost"
        // from the perspective of should_send_headers, so credentials are dropped.
        // The api-key and authorization headers should both be absent.
        assert!(
            first_metadata.get("api-key").is_none(),
            "api-key credential header should be stripped on cleartext non-loopback endpoint"
        );
        assert!(
            first_metadata.get("authorization").is_none(),
            "authorization credential header should be stripped on cleartext non-loopback endpoint"
        );
    });
}

#[test]
#[ignore = "requires a built harnx-fs-tools binary; run after `cargo build -p harnx-fs-tools` with `HARNX_FS_TOOLS_BIN=$PWD/target/debug/harnx-fs-tools cargo nextest run -p harnx-telemetry --all-features --run-ignored ignored-only live_spawned_mcp_tool_server_continues_bridge_carrier`"]
fn live_spawned_mcp_tool_server_continues_bridge_carrier() {
    harnx_core::require_nextest();
    let Some(binary) = live_mcp_binary() else {
        return;
    };

    let (collector_listener, collector_endpoint) = bind_local();
    let _env = EnvGuard::configure(Some(&collector_endpoint));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("live test runtime");

    runtime.block_on(async move {
        let collector = FakeCollector::spawn(collector_listener);
        let telemetry = harnx_telemetry::init_telemetry("harnx-live-test").expect("telemetry init");
        let mut child = tokio::process::Command::new(binary)
            .arg("--mcp-stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn harnx-fs-tools");
        let child_stdin = child.stdin.take().expect("child stdin");
        let child_stdout = child.stdout.take().expect("child stdout");
        let transport = AsyncRwTransport::<RoleClient, _, _>::new(child_stdout, child_stdin);
        let service = rmcp::service::serve_client(LiveMcpClient, transport)
            .await
            .expect("initialize spawned MCP server");
        let peer = service.peer().clone();
        let tools = peer
            .list_tools(Default::default())
            .await
            .expect("list spawned MCP tools");
        let tool_name = tools
            .tools
            .first()
            .expect("at least one fs tool")
            .name
            .clone();

        // This is the exact public injection helper used by harnx-mcp-bridge.
        let tool_call =
            tracing::info_span!(target: "harnx_engine", "tool_call", otel.kind = "client");
        let mut params = CallToolRequestParams::new(tool_name);
        tool_call.in_scope(|| harnx_telemetry::propagate::inject_current_into_mcp(&mut params));
        let propagated_context = harnx_telemetry::propagate::extract_context_from_mcp(&params);
        let (trace_id, span_id) = context_ids(&propagated_context);
        let propagated = PropagatedParent { trace_id, span_id };
        let result = peer
            .call_tool(params)
            .instrument(tool_call.clone())
            .await
            .expect("spawned MCP tool call reached server");
        assert_eq!(
            result.is_error,
            Some(true),
            "argument-free call should fail"
        );
        drop(tool_call);

        service.cancel().await.expect("close MCP client");
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("spawned MCP server shutdown timeout")
            .expect("wait for spawned MCP server");
        assert!(status.success(), "spawned MCP server exited {status}");
        telemetry.shutdown().await;

        let spans = collector.spans();
        assert_live_mcp_trace(&spans, &propagated);
    });
}
