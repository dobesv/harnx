//! Caller-side MRTR round-trip tests (T4.3 Part B). Filter: `mrtr`.
//!
//! Each test builds an in-memory MCP hop: a fake MRTR server on one end of a
//! tokio mpsc channel pair, `BridgeToolset` on the other. This exercises the
//! same dispatch path production uses (the stdio transport is the only
//! difference), without spawning a child process.
#![allow(deprecated)]
use super::*;
use harnx_toolset::Toolset;
use rmcp::model::{
    CallToolRequestParams, CreateMessageRequest, CreateMessageRequestParams, ElicitRequest,
    ElicitRequestParams, InputRequest, InputRequests, ListToolsResult, PaginatedRequestParams,
    ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// What the fake server received on one `tools/call`, exactly as it arrived.
#[derive(Clone, Debug, PartialEq)]
struct RecordedCall {
    arguments: serde_json::Value,
    input_responses: Option<serde_json::Value>,
    request_state: Option<String>,
}

/// Fake MRTR server. `script` decides the response to each call; the recorded
/// calls let tests assert what crossed the wire beside `arguments`.
struct FakeMrtrServer {
    recorded: Arc<Mutex<Vec<RecordedCall>>>,
    script: Vec<rmcp::model::CallToolResponse>,
}

impl FakeMrtrServer {
    fn new(
        recorded: Arc<Mutex<Vec<RecordedCall>>>,
        script: Vec<rmcp::model::CallToolResponse>,
    ) -> Self {
        Self { recorded, script }
    }
}

impl ServerHandler for FakeMrtrServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::model::ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![
            rmcp::model::Tool::new("ask", "Test tool", serde_json::Map::new()),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::model::ErrorData> {
        let call = RecordedCall {
            arguments: serde_json::to_value(request.arguments.clone().unwrap_or_default())
                .expect("arguments serialize"),
            input_responses: request
                .input_responses
                .as_ref()
                .map(|responses| serde_json::to_value(responses).expect("responses serialize")),
            request_state: request.request_state.clone(),
        };
        self.recorded.lock().expect("server state").push(call);
        let index = self.recorded.lock().expect("server state").len() - 1;
        self.script.get(index).cloned().map(Ok).unwrap_or_else(|| {
            Ok(
                rmcp::model::CallToolResult::error(vec![rmcp::model::ContentBlock::text(format!(
                    "script exhausted after {index} call(s)"
                ))])
                .into(),
            )
        })
    }
}

/// A confirm-form MRTR `input_required` response, like a third-party server
/// with no harnx `_meta` would send.
fn form_input_required(
    key: &str,
    message: &str,
    state: Option<String>,
) -> rmcp::model::CallToolResponse {
    let mut requests = InputRequests::new();
    requests.insert(
        key.to_string(),
        InputRequest::Elicitation(ElicitRequest::new(
            ElicitRequestParams::FormElicitationParams {
                meta: None,
                message: message.to_string(),
                requested_schema: serde_json::from_value(
                    json!({"type": "object", "properties": {}}),
                )
                .expect("empty schema"),
            },
        )),
    );
    rmcp::model::InputRequiredResult::new(Some(requests), state).into()
}

/// Wire the fake server and a bridge over an in-memory MCP hop. A single
/// tokio duplex pipe is the transport for both directions: rmcp frames it
/// exactly like the stdio transport production uses.
async fn bridge_over(
    script: Vec<rmcp::model::CallToolResponse>,
) -> (BridgeToolset, Arc<Mutex<Vec<RecordedCall>>>) {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let server = FakeMrtrServer::new(recorded.clone(), script);
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, client_write) = tokio::io::split(client_io);
    let server_transport = rmcp::transport::async_rw::AsyncRwTransport::<RoleServer, _, _>::new(
        server_read,
        server_write,
    );
    let client_transport = rmcp::transport::async_rw::AsyncRwTransport::<RoleClient, _, _>::new(
        client_read,
        client_write,
    );
    tokio::spawn(async move {
        match rmcp::service::serve_server(server, server_transport).await {
            // The returned service drives the connection; keep it alive for
            // the rest of the test (the runtime tears it down at the end).
            Ok(service) => std::mem::forget(service),
            Err(error) => {
                panic!("fake MRTR server failed to initialize: {error:?}");
            }
        }
    });
    // Same lifecycle as production: discover first, so the hop negotiates
    // the MRTR-capable 2026-07-28 revision.
    let lifecycle = rmcp::service::ClientLifecycleMode::Auto {
        preferred_versions: vec![rmcp::model::ProtocolVersion::LATEST],
        // A legacy peer has no discover, so the fallback `initialize` must
        // request a handshake-era revision instead of the discover-era
        // LATEST; the peer answers with its own version either way.
        legacy_version: Some(rmcp::model::ProtocolVersion::V_2025_11_25),
    };
    let client = rmcp::service::serve_client_with_lifecycle(
        BridgeClientHandler,
        client_transport,
        lifecycle,
    )
    .await
    .expect("bridge client initializes against the fake server");
    let bridge = BridgeToolset::from_service(client, "fake")
        .await
        .expect("bridge lists tools over the in-memory hop");
    (bridge, recorded)
}

fn rerun_context(
    responses: Responses,
    request_state: serde_json::Value,
) -> harnx_toolset::ToolInvocationContext {
    harnx_toolset::ToolInvocationContext {
        input_responses: responses,
        request_state,
        ..Default::default()
    }
}

/// Answers keyed by challenge key, the shape the runtime passes on a rerun.
type Responses = std::collections::BTreeMap<String, harnx_toolset::ToolInputResponse>;

fn responses(entries: &[(&str, harnx_toolset::ToolInputResponse)]) -> Responses {
    entries
        .iter()
        .map(|(key, response)| ((*key).to_string(), response.clone()))
        .collect()
}

fn approve_response() -> harnx_toolset::ToolInputResponse {
    harnx_toolset::ToolInputResponse {
        action: "approve".into(),
        data: serde_json::Map::new(),
        note: None,
    }
}

/// An MRTR `input_required` answer suspends the call as a harnx challenge,
/// and the rerun re-calls the server with `inputResponses` and `requestState`
/// beside the unchanged arguments.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mrtr_input_required_suspends_and_resumes_round_trip() {
    let script = vec![
        form_input_required(
            "confirm-key",
            "Proceed?",
            Some(json!({"round": 1}).to_string()),
        ),
        rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text("done")]).into(),
    ];
    let (bridge, recorded) = bridge_over(script).await;

    // MRTR (SEP-2322) is a server-level capability: every tool of a server
    // that negotiated 2026-07-28 is stamped `harnx:input_required`.
    assert!(bridge
        .tools()
        .iter()
        .any(|tool| tool.name == "ask" && tool.requests_input()));

    let suspended = bridge
        .invoke("ask", json!({"x": 1}), CancellationToken::new())
        .await
        .expect_err("first call suspends");
    let ToolInvokeError::InputRequired(challenge) = suspended else {
        panic!("expected a challenge, got {suspended:?}");
    };
    assert_eq!(challenge.kind, "confirm");
    assert_eq!(challenge.key, "confirm-key");
    assert_eq!(challenge.message.as_deref(), Some("Proceed?"));
    assert_eq!(challenge.request_state, json!({"round": 1}));

    let invocation = harnx_toolset::ToolInvocation {
        tool: "ask".into(),
        args: json!({"x": 1}),
        context: rerun_context(
            responses(&[("confirm-key", approve_response())]),
            json!({"round": 1}),
        ),
        cancel: CancellationToken::new(),
    };
    let result = bridge
        .invoke_with_context(invocation)
        .await
        .expect("rerun completes");
    assert_eq!(result["content"][0]["text"], "done");

    let calls = recorded.lock().expect("server state").clone();
    assert_eq!(calls.len(), 2);
    // Answers and state ride beside the arguments, never inside them.
    assert_eq!(calls[0].arguments, json!({"x": 1}));
    assert!(calls[0].input_responses.is_none());
    assert!(calls[0].request_state.is_none());
    assert_eq!(calls[1].arguments, json!({"x": 1}), "arguments unchanged");
    let responses = calls[1]
        .input_responses
        .as_ref()
        .expect("inputResponses on rerun");
    // harnx actions ride as ElicitResult accept/decline for third-party
    // servers, with the original action preserved under `_meta`.
    assert_eq!(responses["confirm-key"]["action"], "accept");
    assert_eq!(responses["confirm-key"]["_meta"]["harnx:action"], "approve");
    assert_eq!(
        calls[1].request_state.as_deref(),
        Some("{\"round\":1}"),
        "requestState echoes the challenge's state"
    );
}

/// Repeated rounds carry only the latest challenge's responses and state
/// (MRTR repeated-challenge rule).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mrtr_repeated_rounds_send_only_latest_responses_and_state() {
    let script = vec![
        form_input_required("step-1", "First?", Some(json!({"round": 1}).to_string())),
        form_input_required("step-2", "Second?", Some(json!({"round": 2}).to_string())),
        rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text("done")]).into(),
    ];
    let (bridge, recorded) = bridge_over(script).await;

    let invocation =
        |responses: Responses, state: serde_json::Value| harnx_toolset::ToolInvocation {
            tool: "ask".into(),
            args: json!({}),
            context: rerun_context(responses, state),
            cancel: CancellationToken::new(),
        };

    let first = bridge
        .invoke_with_context(invocation(Default::default(), serde_json::Value::Null))
        .await
        .expect_err("round 1 suspends");
    let ToolInvokeError::InputRequired(first) = first else {
        panic!("expected a challenge, got {first:?}");
    };
    assert_eq!(first.key, "step-1");

    let second = bridge
        .invoke_with_context(invocation(
            responses(&[("step-1", approve_response())]),
            json!({"round": 1}),
        ))
        .await
        .expect_err("round 2 suspends");
    let ToolInvokeError::InputRequired(second) = second else {
        panic!("expected a challenge, got {second:?}");
    };
    assert_eq!(second.key, "step-2");
    assert_eq!(second.request_state, json!({"round": 2}));

    let result = bridge
        .invoke_with_context(invocation(
            responses(&[("step-2", approve_response())]),
            json!({"round": 2}),
        ))
        .await
        .expect("round 3 completes");
    assert_eq!(result["content"][0]["text"], "done");

    let calls = recorded.lock().expect("server state").clone();
    assert_eq!(calls.len(), 3);
    for (round, call) in calls.iter().enumerate().skip(1) {
        let responses = call
            .input_responses
            .as_ref()
            .expect("responses on every rerun");
        let keys: Vec<&str> = responses
            .as_object()
            .expect("responses object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [format!("step-{round}").as_str()],
            "only the latest challenge's responses arrive"
        );
        assert_eq!(
            call.request_state,
            Some(json!({"round": round}).to_string()),
            "only the latest challenge's state arrives"
        );
    }
}

/// MRTR request kinds harnx cannot show fail with a clear recoverable error,
/// never a silent success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mrtr_unsupported_request_kinds_fail_clearly() {
    // Sampling (createMessage) and roots have no harnx interaction kind.
    let sampling_params: CreateMessageRequestParams = serde_json::from_value(json!({
        "messages": [{"role": "user", "content": {"type": "text", "text": "hi"}}],
        "maxTokens": 16
    }))
    .expect("sampling params");
    let mut sampling = InputRequests::new();
    sampling.insert(
        "s".to_string(),
        InputRequest::CreateMessage(CreateMessageRequest::new(sampling_params)),
    );
    let mut roots = InputRequests::new();
    roots.insert(
        "r".to_string(),
        InputRequest::ListRoots(rmcp::model::ListRootsRequest {
            method: Default::default(),
            extensions: Default::default(),
        }),
    );
    let script = vec![
        rmcp::model::InputRequiredResult::new(Some(sampling), None).into(),
        rmcp::model::InputRequiredResult::new(Some(roots), None).into(),
    ];
    let (bridge, _) = bridge_over(script).await;

    let sampling_error = bridge
        .invoke("ask", json!({}), CancellationToken::new())
        .await
        .expect_err("sampling kind refuses");
    assert!(
        matches!(&sampling_error, ToolInvokeError::Recoverable(message)
            if message.contains("sampling") && message.contains("'s'")),
        "sampling refusal names the request and key: {sampling_error:?}"
    );

    let roots_error = bridge
        .invoke("ask", json!({}), CancellationToken::new())
        .await
        .expect_err("roots kind refuses");
    assert!(
        matches!(&roots_error, ToolInvokeError::Recoverable(message)
            if message.contains("roots") && message.contains("'r'")),
        "roots refusal names the request and key: {roots_error:?}"
    );
}

/// A result with an empty `inputRequests` map is a protocol error a harnx
/// caller can do nothing with; it refuses clearly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mrtr_malformed_input_required_results_fail_clearly() {
    let mut two = InputRequests::new();
    two.insert("a".to_string(), form_request("one"));
    two.insert("b".to_string(), form_request("two"));
    let script = vec![
        rmcp::model::InputRequiredResult::new(Some(two), None).into(),
        rmcp::model::InputRequiredResult::new(Some(InputRequests::new()), None).into(),
    ];
    let (bridge, _) = bridge_over(script).await;

    let multi = bridge
        .invoke("ask", json!({}), CancellationToken::new())
        .await
        .expect_err("two requests at once refuses");
    assert!(
        matches!(&multi, ToolInvokeError::Recoverable(message) if message.contains("2 input requests")),
        "multi-request refusal: {multi:?}"
    );

    let empty = bridge
        .invoke("ask", json!({}), CancellationToken::new())
        .await
        .expect_err("no requests refuses");
    assert!(
        matches!(&empty, ToolInvokeError::Recoverable(message)
            if message.contains("without inputRequests")),
        "missing-requests refusal: {empty:?}"
    );
}

/// A harnx challenge mapped by `mrtr::challenge_to_mrtr` survives the round
/// trip exactly: the `_meta` copy keeps the a2ui surface and message intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mrtr_harnx_meta_preserves_a2ui_challenge() {
    let surface = serde_json::json!({
        "version": "1.0",
        "createSurface": {
            "dataModel": {"name": ""},
            "components": [
                {"id": "root", "component": "Card", "child": "col"},
                {"id": "col", "component": "Column", "children": ["name", "submit"]},
                {"id": "name", "component": "TextField", "label": "Name", "value": {"@path": "/name"}},
                {"id": "submit", "component": "Button", "child": "submit_label", "action": {"event": {"name": "submit"}}}
            ]
        }
    });
    let challenge = harnx_toolset::ToolInputRequired::a2ui(
        "form-key",
        surface.clone(),
        "fill the form",
        json!({"round": 1}),
    );
    let mrtr = harnx_toolset_server::mrtr::challenge_to_mrtr(&challenge).expect("challenge maps");
    let script = vec![rmcp::model::CallToolResponse::InputRequired(mrtr)];
    let (bridge, _) = bridge_over(script).await;

    let suspended = bridge
        .invoke("ask", json!({}), CancellationToken::new())
        .await
        .expect_err("a2ui challenge suspends");
    let ToolInvokeError::InputRequired(decoded) = suspended else {
        panic!("expected a challenge, got {suspended:?}");
    };
    assert_eq!(decoded.kind, "a2ui");
    assert_eq!(decoded.key, "form-key");
    assert_eq!(decoded.summary, "fill the form");
    assert_eq!(decoded.request_state, json!({"round": 1}));
    assert_eq!(decoded.surface.as_ref(), Some(&surface), "surface survives");
}

fn form_request(message: &str) -> InputRequest {
    InputRequest::Elicitation(ElicitRequest::new(
        ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: message.to_string(),
            requested_schema: serde_json::from_value(json!({"type": "object", "properties": {}}))
                .expect("empty schema"),
        },
    ))
}
