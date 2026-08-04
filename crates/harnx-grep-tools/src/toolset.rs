use crate::server::{grep_query_schema, GrepQueryParams, GrepServer};
use async_trait::async_trait;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
use rmcp::model::{CallToolResult, ErrorCode, ErrorData};
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

fn map_result(result: Result<CallToolResult, ErrorData>) -> Result<Value, ToolInvokeError> {
    match result {
        Ok(result) => serde_json::to_value(result).map_err(|err| {
            ToolInvokeError::Fatal(format!("failed to serialize tool result: {err}"))
        }),
        Err(err) if err.code == ErrorCode::INTERNAL_ERROR => {
            Err(ToolInvokeError::Fatal(err.message.to_string()))
        }
        Err(err) => Err(ToolInvokeError::Recoverable(err.message.to_string())),
    }
}

#[async_trait]
impl Toolset for GrepToolset {
    fn name(&self) -> &str {
        "grep"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "grep_query".to_string(),
            description: GREP_QUERY_DESCRIPTION.to_string(),
            input_schema: Value::Object(grep_query_schema()),
            idempotent_hint: true,
            read_only_hint: true,
            timeout_secs: None,
            meta: None,
        }]
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        match tool {
            "grep_query" => {
                let params = parse_args::<GrepQueryParams>(args)?;
                map_result(self.server.grep_query_impl(params).await)
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
    use serde_json::json;

    #[test]
    fn maps_server_errors_by_error_code() {
        let internal = map_result(Err(ErrorData::internal_error("server failed", None)));
        assert!(matches!(internal, Err(ToolInvokeError::Fatal(_))));

        let invalid = map_result(Err(ErrorData::invalid_params("bad input", None)));
        assert!(matches!(invalid, Err(ToolInvokeError::Recoverable(_))));
    }
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const GOLDEN_RESPONSE: &str = include_str!("../tests/fixtures/search_golden_single.json");
    const GOLDEN_OUTPUT: &str = include_str!("../tests/fixtures/search_golden_single_output.txt");

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

        let toolset = GrepToolset::with_base_url(format!("{}/api/search", mock_server.uri()));
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
    }

    #[tokio::test]
    async fn rejects_unknown_tool() {
        let result = GrepToolset::new()
            .invoke("missing", json!({}), CancellationToken::new())
            .await;
        assert!(matches!(result, Err(ToolInvokeError::Recoverable(_))));
    }
}
