use harnx_grep_tools::client::{self, SearchOutcome};
use harnx_grep_tools::format::{build_not_found_output, build_output};
use harnx_grep_tools::server::model::SearchResponse;
use harnx_grep_tools::server::{GrepQueryParams, GrepServer};
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, Implementation,
    InitializeRequestParams,
};
use rmcp::service::{serve_client, serve_server, RoleClient, RoleServer, RunningService};
use serde_json::{Map, Value};
use tokio::io::duplex;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

const GOLDEN_RESPONSE: &str = include_str!("fixtures/search_golden_single.json");
const GOLDEN_OUTPUT: &str = include_str!("fixtures/search_golden_single_output.txt");
const NOT_FOUND_OUTPUT: &str = r#"{
  "query": "somequery",
  "summary": {
    "total_results": 0,
    "message": "No results found for this query"
  },
  "results": []
}"#;

/// Normalize CRLF to LF so byte-exact asserts pass on Windows (where git checks out .txt fixtures as CRLF)
fn golden_output() -> String {
    GOLDEN_OUTPUT.replace("\r\n", "\n")
}

fn endpoint(server: &MockServer) -> String {
    format!("{}/api/search", server.uri())
}

fn params(query: &str) -> GrepQueryParams {
    GrepQueryParams {
        query: query.into(),
        language: None,
        repo: None,
        path: None,
    }
}

#[tokio::test]
async fn client_sends_all_filters_and_formats_success_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/search"))
        .and(query_param("q", "FastAPI"))
        .and(query_param("f.lang", "Python"))
        .and(query_param("f.repo", "fastapi/fastapi"))
        .and(query_param("f.path", "src/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(GOLDEN_RESPONSE, "application/json"))
        .expect(1)
        .mount(&server)
        .await;

    let request = GrepQueryParams {
        query: " FastAPI ".into(),
        language: Some(" Python ".into()),
        repo: Some(" fastapi/fastapi ".into()),
        path: Some(" src/ ".into()),
    };
    let outcome = client::search(&reqwest::Client::new(), &endpoint(&server), &request).await;
    let SearchOutcome::Ok(value) = outcome else {
        panic!("expected successful search outcome");
    };
    let response: SearchResponse = serde_json::from_value(value).expect("fixture must deserialize");
    assert_eq!(build_output("FastAPI", &response), golden_output());
}

#[tokio::test]
async fn client_omits_all_optional_filters() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/search"))
        .and(query_param("q", "rust async"))
        .and(query_param_is_missing("f.lang"))
        .and(query_param_is_missing("f.repo"))
        .and(query_param_is_missing("f.path"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("fixtures/search_nofilters.json"),
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let outcome = client::search(
        &reqwest::Client::new(),
        &endpoint(&server),
        &params("rust async"),
    )
    .await;
    assert!(matches!(outcome, SearchOutcome::Ok(_)));
}

#[tokio::test]
async fn client_maps_404_429_and_500_to_exact_contract_outputs() {
    for (status, expected) in [
        (404, NOT_FOUND_OUTPUT),
        (
            429,
            "❌ Error: Rate limit exceeded. Please wait before making another request.",
        ),
        (500, "❌ Error: API request failed with status 500"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/search"))
            .and(query_param("q", "somequery"))
            .respond_with(ResponseTemplate::new(status))
            .expect(1)
            .mount(&server)
            .await;

        let outcome = client::search(
            &reqwest::Client::new(),
            &endpoint(&server),
            &params("somequery"),
        )
        .await;
        let output = match outcome {
            SearchOutcome::NotFound => build_not_found_output("somequery"),
            SearchOutcome::RateLimited => {
                "❌ Error: Rate limit exceeded. Please wait before making another request.".into()
            }
            SearchOutcome::HttpStatus(code) => {
                format!("❌ Error: API request failed with status {code}")
            }
            other => panic!("unexpected search outcome for status {status}: {other:?}"),
        };
        assert_eq!(output, expected);
    }
}

#[derive(Clone, Default)]
struct TestClientHandler;

impl ClientHandler for TestClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::builder().build(),
            Implementation::new("grep-integration-test", "0.1"),
        )
    }
}

struct TestConnection {
    _server_service: RunningService<RoleServer, GrepServer>,
    client_service: RunningService<RoleClient, TestClientHandler>,
}

async fn connect_server(server: GrepServer) -> TestConnection {
    let (client_transport, server_transport) = duplex(65_536);
    let (server_result, client_result) = tokio::join!(
        serve_server(server, server_transport),
        serve_client(TestClientHandler, client_transport)
    );
    TestConnection {
        _server_service: server_result.expect("server must initialize"),
        client_service: client_result.expect("client must initialize"),
    }
}

fn tool_args(value: Value) -> Map<String, Value> {
    value
        .as_object()
        .expect("tool args must be an object")
        .clone()
}

fn text_content(result: &CallToolResult) -> &str {
    result
        .content
        .iter()
        .find_map(|content| content.as_text().map(|text| text.text.as_str()))
        .expect("tool result must contain text")
}

#[tokio::test]
async fn grep_query_handler_runs_full_pipeline_over_rmcp() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/search"))
        .and(query_param("q", "FastAPI"))
        .and(query_param_is_missing("f.lang"))
        .and(query_param_is_missing("f.repo"))
        .and(query_param_is_missing("f.path"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(GOLDEN_RESPONSE, "application/json"))
        .expect(1)
        .mount(&mock_server)
        .await;

    let connection = connect_server(GrepServer::with_base_url(endpoint(&mock_server))).await;
    let peer = connection.client_service.peer().clone();
    let result = peer
        .call_tool(
            CallToolRequestParams::new("grep_query")
                .with_arguments(tool_args(serde_json::json!({ "query": "FastAPI" }))),
        )
        .await
        .expect("tool call must succeed");

    assert_eq!(result.is_error, Some(false));
    assert_eq!(text_content(&result), golden_output());
}
