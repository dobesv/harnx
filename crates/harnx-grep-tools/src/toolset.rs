use crate::script_schema::{grep_output_schema, grep_script_output};
use crate::server::{grep_query_schema, GrepQueryParams, GrepServer};
use async_trait::async_trait;
use harnx_toolset::{ToolInvocation, ToolInvokeError, ToolProgressKind, ToolSpec, Toolset};
use rmcp::model::{CallToolResult, ErrorData};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

const GREP_QUERY_DESCRIPTION: &str = "Search GitHub code using grep.app API. This tool enables AI assistants to search through GitHub repositories for specific code patterns using grep.app's powerful search index. It returns formatted results with repository information, file paths, and code snippets.";

/// Native toolset for grep.app-backed GitHub code search.
///
/// Both native NATS calls and the `--mcp-stdio` adapter use the same
/// [`GrepServer`] search and formatting path.
#[derive(Clone)]
pub struct GrepToolset {
    server: GrepServer,
}

impl GrepToolset {
    pub fn new() -> Self {
        Self {
            server: GrepServer::new(),
        }
    }

    /// Creates a toolset targeting a custom grep.app-compatible endpoint.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            server: GrepServer::with_base_url(base_url),
        }
    }
}

impl Default for GrepToolset {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_args<T: DeserializeOwned>(args: Value) -> Result<T, ToolInvokeError> {
    serde_json::from_value(args)
        .map_err(|err| ToolInvokeError::Recoverable(format!("invalid tool arguments: {err}")))
}

/// The one tool this server publishes, shared by the native toolset and the
/// direct MCP handler so their listings stay in parity.
pub(crate) fn grep_query_spec() -> ToolSpec {
    ToolSpec {
        cancellation_guarantee: Default::default(),
        name: "grep_query".to_string(),
        description: GREP_QUERY_DESCRIPTION.to_string(),
        input_schema: Value::Object(grep_query_schema()),
        idempotent_hint: true,
        read_only_hint: true,
        timeout_secs: None,
        meta: None,
    }
    .with_kind(ToolProgressKind::Search)
    .with_script_output(grep_script_output())
    .with_output_schema(grep_output_schema())
}

fn map_result(result: Result<CallToolResult, ErrorData>) -> Result<Value, ToolInvokeError> {
    match result {
        Ok(result) => serde_json::to_value(result).map_err(|err| {
            ToolInvokeError::Fatal(format!("failed to serialize tool result: {err}"))
        }),
        Err(err) => Err(ToolInvokeError::Recoverable(err.message.to_string())),
    }
}

#[async_trait]
impl Toolset for GrepToolset {
    fn name(&self) -> &str {
        "grep"
    }

    fn default_mcp_http_port(&self) -> u16 {
        3004
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![grep_query_spec()]
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        self.invoke_styled(tool, args, false, cancel).await
    }

    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        let structured = invocation.context.wants_structured_result();
        self.invoke_styled(
            &invocation.tool,
            invocation.args,
            structured,
            invocation.cancel,
        )
        .await
    }
}

impl GrepToolset {
    /// One dispatch for every caller style; `structured` requests the whole
    /// result document as `structuredContent`, as a script's inner call does.
    async fn invoke_styled(
        &self,
        tool: &str,
        args: Value,
        structured: bool,
        _cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        match tool {
            "grep_query" => {
                let params = parse_args::<GrepQueryParams>(args)?;
                map_result(self.server.grep_query_impl(params, structured).await)
            }
            _ => Err(ToolInvokeError::Recoverable(format!(
                "unknown grep tool: {tool}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_toolset::{ScriptOutput, ToolInvocationContext, CAPABILITY_STRUCTURED_RESULT};
    use serde_json::json;

    #[test]
    fn maps_all_server_errors_to_recoverable() {
        let internal = map_result(Err(ErrorData::internal_error("server failed", None)));
        assert!(matches!(internal, Err(ToolInvokeError::Recoverable(_))));

        let invalid = map_result(Err(ErrorData::invalid_params("bad input", None)));
        assert!(matches!(invalid, Err(ToolInvokeError::Recoverable(_))));
    }
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const GOLDEN_RESPONSE: &str = include_str!("../tests/fixtures/search_golden_single.json");
    const GOLDEN_OUTPUT: &str = include_str!("../tests/fixtures/search_golden_single_output.txt");

    /// The 404 report pretty-printed exactly as a direct call prints it.
    const NOT_FOUND_PRETTY: &str = r#"{
  "query": "missing",
  "summary": {
    "total_results": 0,
    "message": "No results found for this query"
  },
  "results": []
}"#;

    /// Points the toolset at the mock server's search endpoint.
    fn toolset_for(mock_server: &MockServer) -> GrepToolset {
        GrepToolset::with_base_url(format!("{}/api/search", mock_server.uri()))
    }

    #[test]
    fn exposes_grep_query_spec() {
        let toolset = GrepToolset::new();
        assert_eq!(toolset.name(), "grep");

        let tools = toolset.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "grep_query");
        assert!(tools[0].idempotent_hint);
        assert!(tools[0].read_only_hint);
        assert_eq!(tools[0].input_schema["type"], json!("object"));
        assert_eq!(tools[0].input_schema["required"], json!(["query"]));
        assert_eq!(
            tools[0].script_output().expect("script output"),
            ScriptOutput {
                summary: Some("the search report as JSON; pipe it to jq".to_string()),
                ..ScriptOutput::default()
            },
        );
        let schema = tools[0].output_schema().expect("output schema");
        for field in ["query", "summary", "results_by_repository", "results"] {
            assert!(
                schema["properties"].get(field).is_some(),
                "output schema lacks {field}"
            );
        }
    }

    #[tokio::test]
    async fn invokes_existing_search_and_format_pipeline() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/search"))
            .and(query_param("q", "FastAPI"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(GOLDEN_RESPONSE, "application/json"),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let toolset = toolset_for(&mock_server);
        let result = toolset
            .invoke(
                "grep_query",
                json!({"query": "FastAPI"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(result["isError"], json!(false));
        assert_eq!(result["content"][0]["text"], json!(GOLDEN_OUTPUT));
        assert!(result.get("structuredContent").is_none(), "{result}");
    }

    fn script_invocation(tool: &str, args: Value) -> ToolInvocation {
        let mut context = ToolInvocationContext::default();
        context
            .capabilities
            .insert(CAPABILITY_STRUCTURED_RESULT.to_string());
        ToolInvocation {
            tool: tool.to_string(),
            args,
            context,
            cancel: CancellationToken::new(),
        }
    }

    /// A contextual invocation whose caller did not opt in to structured
    /// results, the context ordinary model calls run under.
    fn plain_invocation(tool: &str, args: Value) -> ToolInvocation {
        ToolInvocation {
            tool: tool.to_string(),
            args,
            context: ToolInvocationContext::default(),
            cancel: CancellationToken::new(),
        }
    }

    async fn mount_search_response(mock_server: &MockServer, query: &'static str, status: u16) {
        let response = if status == 200 {
            ResponseTemplate::new(200).set_body_raw(GOLDEN_RESPONSE, "application/json")
        } else {
            ResponseTemplate::new(404)
        };
        Mock::given(method("GET"))
            .and(path("/api/search"))
            .and(query_param("q", query))
            .respond_with(response)
            .expect(1)
            .mount(mock_server)
            .await;
    }

    #[tokio::test]
    async fn script_invocation_returns_the_whole_document_for_hits() {
        let mock_server = MockServer::start().await;
        mount_search_response(&mock_server, "FastAPI", 200).await;

        let toolset = toolset_for(&mock_server);
        let result = toolset
            .invoke_with_context(script_invocation("grep_query", json!({"query": "FastAPI"})))
            .await
            .unwrap();

        assert_eq!(result["isError"], json!(false));
        let document = &result["structuredContent"];
        let expected: serde_json::Value = serde_json::from_str(GOLDEN_OUTPUT).unwrap();
        assert_eq!(document, &expected, "{result}");
        assert_eq!(result["content"][0]["text"], json!(document.to_string()));
        assert_schema_covers_the_document(&result);
    }

    #[tokio::test]
    async fn script_invocation_returns_the_whole_document_for_no_results() {
        let mock_server = MockServer::start().await;
        mount_search_response(&mock_server, "missing", 404).await;

        let toolset = toolset_for(&mock_server);
        let result = toolset
            .invoke_with_context(script_invocation("grep_query", json!({"query": "missing"})))
            .await
            .unwrap();

        assert_eq!(result["isError"], json!(false));
        let document = &result["structuredContent"];
        assert_eq!(document["query"], json!("missing"));
        assert_eq!(
            document["summary"]["message"],
            json!("No results found for this query")
        );
        assert_eq!(document["results"], json!([]));
        assert_schema_covers_the_document(&result);
    }

    /// Without the structured-result capability the contextual entry point
    /// keeps the direct-call pretty text, for both result shapes.
    #[tokio::test]
    async fn context_invocation_without_capability_keeps_pretty_text() {
        for (query, status, expected) in [
            ("FastAPI", 200u16, GOLDEN_OUTPUT),
            ("missing", 404, NOT_FOUND_PRETTY),
        ] {
            let mock_server = MockServer::start().await;
            mount_search_response(&mock_server, query, status).await;

            let toolset = toolset_for(&mock_server);
            let result = toolset
                .invoke_with_context(plain_invocation("grep_query", json!({"query": query})))
                .await
                .unwrap();

            assert_eq!(result["isError"], json!(false), "{query}");
            assert!(
                result.get("structuredContent").is_none(),
                "{query}: {result}"
            );
            assert_eq!(result["content"][0]["text"], json!(expected), "{query}");
        }
    }

    #[tokio::test]
    async fn script_invocation_keeps_errors_as_text() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429))
            .expect(1)
            .mount(&mock_server)
            .await;

        let toolset = toolset_for(&mock_server);
        let result = toolset
            .invoke_with_context(script_invocation("grep_query", json!({"query": "test"})))
            .await
            .unwrap();

        assert_eq!(result["isError"], json!(true));
        assert!(result.get("structuredContent").is_none(), "{result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Rate limit"), "{text}");
    }

    /// Every top-level key of the structured document is declared in the
    /// spec's output schema, so `help grep_query` documents what exists.
    fn assert_schema_covers_the_document(result: &serde_json::Value) {
        let spec = GrepToolset::new().tools().remove(0);
        let schema = spec.output_schema().expect("output schema");
        let properties = schema["properties"].as_object().expect("schema properties");
        for key in result["structuredContent"]
            .as_object()
            .expect("structured document")
            .keys()
        {
            assert!(properties.contains_key(key), "undeclared field {key}");
        }
    }

    #[tokio::test]
    async fn rejects_unknown_tool() {
        let result = GrepToolset::new()
            .invoke("missing", json!({}), CancellationToken::new())
            .await;
        assert!(matches!(result, Err(ToolInvokeError::Recoverable(_))));
    }
}
