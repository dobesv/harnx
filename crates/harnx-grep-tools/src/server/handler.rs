use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
    Tool, ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde_json::{Map, Value};

use crate::client::{self, SearchOutcome};
use crate::format;

use super::model::SearchResponse;
use super::{GrepQueryParams, GrepServer};

const QUERY_REQUIRED_ERROR: &str =
    "❌ Error: 'query' parameter is required and must be a non-empty string";
const RATE_LIMIT_ERROR: &str =
    "❌ Error: Rate limit exceeded. Please wait before making another request.";
const TIMEOUT_ERROR: &str =
    "❌ Error: Request timed out. The grep.app API may be experiencing issues.";

fn metric_tool_name(tool: &str) -> &str {
    if tool == "grep_query" {
        tool
    } else {
        "unknown"
    }
}

fn tool_call_succeeded(result: &Result<CallToolResult, ErrorData>) -> bool {
    result
        .as_ref()
        .is_ok_and(|result| result.is_error != Some(true))
}

/// Converts a domain/inner error into an isError result.
///
/// This wrapper maps `Err(ErrorData)` to `Ok(CallToolResult::error(...))`,
/// ensuring recoverable failures are returned as `is_error: Some(true)`
/// instead of JSON-RPC error frames. Used at the `dispatch_call_tool`
/// boundary for known-tool arms.
fn domain_result(result: Result<CallToolResult, ErrorData>) -> Result<CallToolResult, ErrorData> {
    match result {
        Ok(ok) => Ok(ok),
        Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e.message)])),
    }
}

fn error_result(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(text.into())])
}

fn text_result(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text.into())])
}

impl GrepServer {
    /// Runs one search and returns either the whole report document as
    /// `structuredContent` (`structured`, a script's inner call) or the same
    /// document pretty-printed as text (a direct model call). Errors are
    /// unchanged by the request style.
    pub async fn grep_query_impl(
        &self,
        params: GrepQueryParams,
        structured: bool,
    ) -> Result<CallToolResult, ErrorData> {
        if let Err(message) = params.validate() {
            return Ok(error_result(message));
        }

        let query = params.query.trim();
        let document = match client::search(&self.client, &self.base_url, &params).await {
            SearchOutcome::Ok(value) => match serde_json::from_value::<SearchResponse>(value) {
                Ok(response) => format::build_search_document(query, &response),
                Err(error) => return Ok(error_result(unexpected_response_error(error))),
            },
            SearchOutcome::NotFound => format::build_not_found_document(query),
            SearchOutcome::RateLimited => {
                return Ok(error_result(format!(
                    "{}\n\nRetry guidance: wait 60 seconds before retrying.",
                    RATE_LIMIT_ERROR
                )));
            }
            SearchOutcome::HttpStatus(status) => {
                return Ok(error_result(format!(
                    "❌ Error: API request failed with status {status}"
                )));
            }
            SearchOutcome::Timeout => {
                return Ok(error_result(format!(
                    "{}\n\nTry a narrower search scope or a more specific pattern.",
                    TIMEOUT_ERROR
                )));
            }
            SearchOutcome::Malformed(error) => {
                return Ok(error_result(unexpected_response_error(error)));
            }
            SearchOutcome::Network(details) => {
                return Ok(error_result(format!(
                    "❌ Error: Network error while contacting grep.app API: {details}"
                )));
            }
        };

        Ok(if structured {
            CallToolResult::structured(document)
        } else {
            text_result(format::render_document(&document))
        })
    }
}

impl ServerHandler for GrepServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "harnx-grep-tools",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions("Search GitHub code through the grep.app search index.")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // Built from the same ToolSpec the native toolset publishes, so the
        // script-output and output-schema `_meta` reaches direct MCP clients
        // exactly as it reaches them through the MCP adapter.
        let spec = crate::toolset::grep_query_spec();
        let input_schema = match spec.input_schema {
            Value::Object(schema) => schema,
            _ => Map::new(),
        };
        let mut grep_query = Tool::new(spec.name, spec.description, input_schema).annotate(
            ToolAnnotations::new()
                .read_only(true)
                .destructive(false)
                .idempotent(true)
                .open_world(true),
        );
        grep_query.meta = spec.meta.map(rmcp::model::MetaObject);

        Ok(ListToolsResult::with_all_items(vec![grep_query]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let tool = request.name.clone();
        let metric_tool = metric_tool_name(&tool);
        let start = std::time::Instant::now();
        let result = self.dispatch_call_tool(request, _context).await;
        let elapsed = start.elapsed();
        let is_ok = tool_call_succeeded(&result);
        harnx_metrics::record_tool_call(metric_tool, is_ok, elapsed);
        result.map(Into::into)
    }
}

impl GrepServer {
    /// The tool dispatch, which always finishes in a single step.
    ///
    /// `call_tool` must return `CallToolResponse`, whose other variants cover
    /// elicitation and long-running tasks that this server does not use.
    /// Dispatching separately keeps every arm returning a plain
    /// `CallToolResult`.
    async fn dispatch_call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        match request.name.as_ref() {
            "grep_query" => domain_result(
                async {
                    let structured = structured_result_requested(&context);
                    let params = parse_arguments::<GrepQueryParams>(request.arguments)?;
                    self.grep_query_impl(params, structured).await
                }
                .await,
            ),
            name => Err(ErrorData::invalid_params(
                format!("unknown tool: {name}"),
                None,
            )),
        }
    }
}

/// Whether the caller asked for the result as `structuredContent`, the same
/// `_meta` signal `McpToolsetAdapter` translates into the structured-result
/// capability.
fn structured_result_requested(context: &RequestContext<RoleServer>) -> bool {
    context
        .meta
        .get(harnx_toolset::CAPABILITY_STRUCTURED_RESULT)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn unexpected_response_error(error: impl std::fmt::Display) -> String {
    format!("❌ Error: Unexpected response format from grep.app API: {error}")
}

pub(crate) fn grep_query_schema() -> Map<String, Value> {
    let mut properties = Map::new();
    properties.insert(
        "query".to_string(),
        string_property("The search query string to find in GitHub repositories"),
    );
    properties.insert(
        "language".to_string(),
        string_property("Optional programming language filter (e.g., \"Python\", \"JavaScript\")"),
    );
    properties.insert(
        "repo".to_string(),
        string_property(
            "Optional repository filter in format \"owner/repo\" (e.g., \"fastapi/fastapi\")",
        ),
    );
    properties.insert(
        "path".to_string(),
        string_property(
            "Optional path filter to search within specific directories (e.g., \"src/\")",
        ),
    );

    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));
    schema.insert("properties".to_string(), Value::Object(properties));
    schema.insert(
        "required".to_string(),
        Value::Array(vec![Value::String("query".to_string())]),
    );
    schema
}

fn string_property(description: &str) -> Value {
    let mut property = Map::new();
    property.insert("type".to_string(), Value::String("string".to_string()));
    property.insert(
        "description".to_string(),
        Value::String(description.to_string()),
    );
    Value::Object(property)
}

/// Parse tool arguments from the request.
fn parse_arguments<T: serde::de::DeserializeOwned>(
    arguments: Option<Map<String, Value>>,
) -> Result<T, ErrorData> {
    let args = arguments.unwrap_or_default();
    serde_json::from_value::<T>(Value::Object(args))
        .map_err(|e| ErrorData::invalid_params(format!("{}: {}", QUERY_REQUIRED_ERROR, e), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_tool_name_returns_grep_query_for_known_tool() {
        assert_eq!(metric_tool_name("grep_query"), "grep_query");
    }

    #[test]
    fn metric_tool_name_returns_unknown_for_other_tools() {
        assert_eq!(metric_tool_name("other"), "unknown");
        assert_eq!(metric_tool_name("list_plans"), "unknown");
    }

    #[test]
    fn tool_call_succeeded_returns_true_for_ok_without_is_error() {
        let result: Result<CallToolResult, ErrorData> = Ok(text_result("success"));
        assert!(tool_call_succeeded(&result));
    }

    #[test]
    fn tool_call_succeeded_returns_false_for_ok_with_is_error() {
        let result: Result<CallToolResult, ErrorData> = Ok(error_result("error"));
        assert!(!tool_call_succeeded(&result));
    }

    #[test]
    fn tool_call_succeeded_returns_false_for_err() {
        let result: Result<CallToolResult, ErrorData> =
            Err(ErrorData::internal_error("test".to_string(), None));
        assert!(!tool_call_succeeded(&result));
    }

    #[test]
    fn error_result_sets_is_error_true() {
        let result = error_result("test error");
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn text_result_sets_is_error_none() {
        let result = text_result("test success");
        assert!(result.is_error.is_none() || result.is_error == Some(false));
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use rmcp::handler::client::ClientHandler;
    use rmcp::model::{ClientCapabilities, InitializeRequestParams};
    use rmcp::service::{serve_client, serve_server, RoleClient, RoleServer, RunningService};
    use serde_json::json;
    use tokio::io::duplex;

    #[derive(Clone, Default)]
    struct TestClientHandler;

    impl ClientHandler for TestClientHandler {
        fn get_info(&self) -> InitializeRequestParams {
            InitializeRequestParams::new(
                ClientCapabilities::builder().build(),
                Implementation::new("test", "0.1"),
            )
        }
    }

    type TestServerService = RunningService<RoleServer, GrepServer>;
    type TestClientService = RunningService<RoleClient, TestClientHandler>;

    async fn setup_client_server() -> (TestClientService, TestServerService) {
        setup_client_server_for(GrepServer::new()).await
    }

    /// Generic over the served handler so tests can drive `GrepServer` and the
    /// MCP adapter through the same in-memory transport.
    async fn setup_client_server_for<S>(
        server: S,
    ) -> (TestClientService, RunningService<RoleServer, S>)
    where
        S: rmcp::ServerHandler + 'static,
    {
        let (client_transport, server_transport) = duplex(65_536);

        let server_fut = serve_server(server, server_transport);
        let client_fut = serve_client(TestClientHandler, client_transport);

        let (server_res, client_res): (
            Result<RunningService<RoleServer, S>, _>,
            Result<TestClientService, _>,
        ) = tokio::join!(server_fut, client_fut);

        let server = server_res.unwrap();
        let client = client_res.unwrap();
        (client, server)
    }

    /// Asserts that a known-tool domain failure returns `Ok` with `is_error: Some(true)`
    fn assert_is_error_result(
        result: &Result<CallToolResult, rmcp::service::ServiceError>,
        contains: &str,
    ) {
        match result {
            Ok(result) => {
                assert!(
                    result.is_error == Some(true),
                    "expected is_error: Some(true), got {:?}",
                    result.is_error
                );
                let text = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                assert!(
                    text.contains(contains),
                    "expected content to contain {:?}, got {:?}",
                    contains,
                    text
                );
            }
            Err(e) => panic!("expected Ok result, got Err: {:?}", e),
        }
    }

    /// Asserts that an unknown-tool call returns a protocol error frame `Err(ServiceError)`.
    fn assert_protocol_error(result: &Result<CallToolResult, rmcp::service::ServiceError>) {
        assert!(
            result.is_err(),
            "expected Err (protocol error) for unknown tool, got Ok: {:?}",
            result
        );
    }

    mod with_wiremock {
        use super::*;
        use rmcp::model::CallToolRequestParams;
        use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

        async fn setup_client_server_with_mock(
            mock_server: &MockServer,
        ) -> (TestClientService, TestServerService) {
            setup_client_server_for(GrepServer::with_base_url(mock_server.uri())).await
        }

        #[tokio::test]
        async fn invalid_arguments_returns_is_error() {
            let (client, _server) = setup_client_server().await;
            let peer = client.peer();

            // Call grep_query with missing required argument 'query'
            let result = peer
                .call_tool(
                    CallToolRequestParams::new("grep_query").with_arguments(
                        json!({ "language": "Python" }).as_object().unwrap().clone(),
                    ),
                )
                .await;

            // Argument validation errors should return Ok with is_error: true
            assert_is_error_result(&result, "query");

            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn empty_arguments_returns_is_error() {
            let (client, _server) = setup_client_server().await;
            let peer = client.peer();

            let result = peer
                .call_tool(CallToolRequestParams::new("grep_query"))
                .await;

            assert_is_error_result(&result, "query");

            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn rate_limit_returns_is_error() {
            let mock_server = MockServer::start().await;
            mock_server
                .register(
                    Mock::given(matchers::method("GET")).respond_with(ResponseTemplate::new(429)),
                )
                .await;

            let (client, _server) = setup_client_server_with_mock(&mock_server).await;
            let peer = client.peer();

            let result = peer
                .call_tool(
                    CallToolRequestParams::new("grep_query")
                        .with_arguments(json!({ "query": "test" }).as_object().unwrap().clone()),
                )
                .await;

            assert_is_error_result(&result, "Rate limit");
            assert_is_error_result(&result, "Retry guidance");

            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn http_status_error_returns_is_error() {
            let mock_server = MockServer::start().await;
            mock_server
                .register(
                    Mock::given(matchers::method("GET")).respond_with(ResponseTemplate::new(500)),
                )
                .await;

            let (client, _server) = setup_client_server_with_mock(&mock_server).await;
            let peer = client.peer();

            let result = peer
                .call_tool(
                    CallToolRequestParams::new("grep_query")
                        .with_arguments(json!({ "query": "test" }).as_object().unwrap().clone()),
                )
                .await;

            assert_is_error_result(&result, "status 500");

            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn timeout_returns_is_error() {
            let mock_server = MockServer::start().await;
            mock_server
                .register(Mock::given(matchers::method("GET")).respond_with(
                    ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(10)),
                ))
                .await;

            // A short timeout exercises the same path as the production 30s
            // one without the test waiting it out.
            let server =
                GrepServer::with_timeout(mock_server.uri(), std::time::Duration::from_millis(200));
            let (client, _server) = setup_client_server_for(server).await;
            let peer = client.peer();

            let result = peer
                .call_tool(
                    CallToolRequestParams::new("grep_query")
                        .with_arguments(json!({ "query": "test" }).as_object().unwrap().clone()),
                )
                .await;

            assert_is_error_result(&result, "timed out");
            assert_is_error_result(&result, "narrower");

            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn malformed_response_returns_is_error() {
            let mock_server = MockServer::start().await;
            mock_server
                .register(
                    Mock::given(matchers::method("GET"))
                        .respond_with(ResponseTemplate::new(200).set_body_string("not json")),
                )
                .await;

            let (client, _server) = setup_client_server_with_mock(&mock_server).await;
            let peer = client.peer();

            let result = peer
                .call_tool(
                    CallToolRequestParams::new("grep_query")
                        .with_arguments(json!({ "query": "test" }).as_object().unwrap().clone()),
                )
                .await;

            assert_is_error_result(&result, "Unexpected response format");

            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn successful_search_returns_ok_without_is_error() {
            let mock_server = MockServer::start().await;
            mock_server
                .register(Mock::given(matchers::method("GET")).respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({
                        "hits": {
                            "hits": []
                        }
                    })),
                ))
                .await;

            let (client, _server) = setup_client_server_with_mock(&mock_server).await;
            let peer = client.peer();

            let result = peer
                .call_tool(
                    CallToolRequestParams::new("grep_query")
                        .with_arguments(json!({ "query": "test" }).as_object().unwrap().clone()),
                )
                .await;

            match result {
                Ok(result) => {
                    assert!(
                        result.is_error != Some(true),
                        "successful call should not have is_error: true"
                    );
                }
                Err(e) => {
                    panic!("successful call should return Ok, got Err: {:?}", e);
                }
            }

            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn not_found_returns_ok_without_is_error() {
            let mock_server = MockServer::start().await;
            mock_server
                .register(
                    Mock::given(matchers::method("GET")).respond_with(ResponseTemplate::new(404)),
                )
                .await;

            let (client, _server) = setup_client_server_with_mock(&mock_server).await;
            let peer = client.peer();

            // 404 means "no matches", which is NOT an error
            let result = peer
                .call_tool(
                    CallToolRequestParams::new("grep_query")
                        .with_arguments(json!({ "query": "test" }).as_object().unwrap().clone()),
                )
                .await;

            // NotFound should NOT be an error - it returns formatted not-found output
            match result {
                Ok(result) => {
                    assert!(
                        result.is_error != Some(true),
                        "not-found (404) should not be an error, got is_error: {:?}",
                        result.is_error
                    );
                }
                Err(e) => {
                    panic!("not-found should return Ok, got Err: {:?}", e);
                }
            }

            client.cancel().await.unwrap();
        }
    }

    #[tokio::test]
    async fn unknown_tool_returns_protocol_error() {
        let (client, _server) = setup_client_server().await;
        let peer = client.peer();

        let result = peer
            .call_tool(CallToolRequestParams::new("unknown_tool"))
            .await;

        // Unknown tools should return Err (protocol error frame)
        assert_protocol_error(&result);

        client.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn unknown_tool_error_is_invalid_params() {
        let (client, _server) = setup_client_server().await;
        let peer = client.peer();

        let result = peer
            .call_tool(CallToolRequestParams::new("nonexistent"))
            .await;

        // The error should be a protocol error (Err), not a domain error
        assert!(result.is_err());
        let err = result.unwrap_err();
        // The error message should mention "invalid" or "params" or "unknown tool"
        let err_string = format!("{:?}", err);
        assert!(
            err_string.contains("invalid")
                || err_string.contains("params")
                || err_string.contains("unknown tool"),
            "expected invalid_params error, got: {:?}",
            err_string
        );

        client.cancel().await.unwrap();
    }

    mod script_results {
        use super::*;
        use harnx_toolset_server::McpToolsetAdapter;
        use std::sync::Arc;
        use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

        const GOLDEN_RESPONSE: &str =
            include_str!("../../tests/fixtures/search_golden_single.json");
        const GOLDEN_OUTPUT: &str =
            include_str!("../../tests/fixtures/search_golden_single_output.txt");

        /// Normalize CRLF so byte-exact asserts pass on Windows, where git
        /// checks .txt fixtures out with CRLF.
        fn golden_output() -> String {
            GOLDEN_OUTPUT.replace("\r\n", "\n")
        }

        fn search_url(mock: &MockServer) -> String {
            format!("{}/api/search", mock.uri())
        }

        async fn mount_hits(server: &MockServer) {
            Mock::given(matchers::method("GET"))
                .and(matchers::path("/api/search"))
                .and(matchers::query_param("q", "FastAPI"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_raw(GOLDEN_RESPONSE, "application/json"),
                )
                .mount(server)
                .await;
        }

        async fn mount_not_found(server: &MockServer) {
            Mock::given(matchers::method("GET"))
                .and(matchers::path("/api/search"))
                .respond_with(ResponseTemplate::new(404))
                .mount(server)
                .await;
        }

        fn structured_call(query: &str) -> CallToolRequestParams {
            let mut params = CallToolRequestParams::new("grep_query");
            params.meta.get_or_insert_default().insert(
                harnx_toolset::CAPABILITY_STRUCTURED_RESULT.to_owned(),
                Value::Bool(true),
            );
            params.arguments = Some(
                json!({ "query": query })
                    .as_object()
                    .expect("arguments object")
                    .clone(),
            );
            params
        }

        fn plain_call(query: &str) -> CallToolRequestParams {
            CallToolRequestParams::new("grep_query").with_arguments(
                json!({ "query": query })
                    .as_object()
                    .expect("arguments object")
                    .clone(),
            )
        }

        #[tokio::test]
        async fn listing_meta_matches_the_native_spec_and_the_mcp_adapter() {
            let (grep_client, _grep_server) = setup_client_server().await;
            let listed = grep_client
                .peer()
                .list_tools(None)
                .await
                .expect("grep listing");
            let direct_meta = listed.tools.first().expect("one tool").meta.clone();
            grep_client.cancel().await.unwrap();

            let adapter = McpToolsetAdapter {
                toolset: Arc::new(crate::GrepToolset::new()),
            };
            let (adapter_client, _adapter_server) = setup_client_server_for(adapter).await;
            let listed = adapter_client
                .peer()
                .list_tools(None)
                .await
                .expect("adapter listing");
            let adapter_meta = listed.tools.first().expect("one tool").meta.clone();
            adapter_client.cancel().await.unwrap();

            let spec = crate::toolset::grep_query_spec();
            let native = serde_json::to_value(spec.meta).expect("spec meta");
            let direct = serde_json::to_value(direct_meta).expect("direct meta");
            let adapted = serde_json::to_value(adapter_meta).expect("adapter meta");
            assert_eq!(direct, native, "GrepServer listing must match the spec");
            assert_eq!(
                adapted, native,
                "McpToolsetAdapter listing must match the spec"
            );
            for key in [
                harnx_toolset::SCRIPT_OUTPUT_META_KEY,
                harnx_toolset::OUTPUT_SCHEMA_META_KEY,
            ] {
                assert!(direct.get(key).is_some(), "listing lacks {key}");
            }
        }

        #[tokio::test]
        async fn structured_request_returns_the_whole_document_over_mcp() {
            let mock = MockServer::start().await;
            mount_hits(&mock).await;
            let server = GrepServer::with_base_url(search_url(&mock));
            let (client, _server) = setup_client_server_for(server).await;
            let result = client
                .peer()
                .call_tool(structured_call("FastAPI"))
                .await
                .expect("tool call");

            assert_ne!(result.is_error, Some(true));
            let document = result
                .structured_content
                .clone()
                .expect("structured content");
            let expected: Value =
                serde_json::from_str(&golden_output()).expect("golden output is JSON");
            assert_eq!(document, expected, "{result:?}");
            let text = result
                .content
                .iter()
                .find_map(|block| block.as_text().map(|text| text.text.clone()))
                .expect("text block");
            assert_eq!(text, document.to_string());
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn plain_request_still_prints_the_pretty_document() {
            let mock = MockServer::start().await;
            mount_hits(&mock).await;
            let server = GrepServer::with_base_url(search_url(&mock));
            let (client, _server) = setup_client_server_for(server).await;
            let result = client
                .peer()
                .call_tool(plain_call("FastAPI"))
                .await
                .expect("tool call");

            assert!(result.structured_content.is_none(), "{result:?}");
            let text = result
                .content
                .iter()
                .find_map(|block| block.as_text().map(|text| text.text.clone()))
                .expect("text block");
            let expected = golden_output();
            assert_eq!(text, expected);
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn structured_request_returns_the_not_found_document_over_mcp() {
            let mock = MockServer::start().await;
            mount_not_found(&mock).await;
            let server = GrepServer::with_base_url(search_url(&mock));
            let (client, _server) = setup_client_server_for(server).await;
            let result = client
                .peer()
                .call_tool(structured_call("missing"))
                .await
                .expect("tool call");

            let document = result.structured_content.expect("structured content");
            assert_eq!(document["query"], json!("missing"));
            assert_eq!(
                document["summary"]["message"],
                json!("No results found for this query")
            );
            assert_eq!(document["results"], json!([]));
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn adapter_call_matches_the_direct_server_structured_result() {
            let mock = MockServer::start().await;
            mount_hits(&mock).await;
            let endpoint = search_url(&mock);

            let adapter = McpToolsetAdapter {
                toolset: Arc::new(crate::GrepToolset::with_base_url(endpoint.clone())),
            };
            let (client, _server) = setup_client_server_for(adapter).await;
            let adapted = client
                .peer()
                .call_tool(structured_call("FastAPI"))
                .await
                .expect("adapter call");
            client.cancel().await.unwrap();

            let (direct_client, _direct_server) =
                setup_client_server_for(GrepServer::with_base_url(endpoint)).await;
            let direct = direct_client
                .peer()
                .call_tool(structured_call("FastAPI"))
                .await
                .expect("direct call");
            direct_client.cancel().await.unwrap();

            assert_eq!(adapted.structured_content, direct.structured_content);
            assert_eq!(adapted.is_error, direct.is_error);
        }
    }
}
