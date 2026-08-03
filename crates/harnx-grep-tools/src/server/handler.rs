use rmcp::model::{
    CallToolRequestMethod, CallToolRequestParams, CallToolResult, ContentBlock, ErrorData,
    Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    ToolAnnotations,
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

impl GrepServer {
    pub async fn grep_query_impl(
        &self,
        params: GrepQueryParams,
    ) -> Result<CallToolResult, ErrorData> {
        if let Err(message) = params.validate() {
            return Ok(text_result(message));
        }

        let query = params.query.trim();
        let output = match client::search(&self.client, &self.base_url, &params).await {
            SearchOutcome::Ok(value) => match serde_json::from_value::<SearchResponse>(value) {
                Ok(response) => format::build_output(query, &response),
                Err(error) => unexpected_response_error(error),
            },
            SearchOutcome::NotFound => format::build_not_found_output(query),
            SearchOutcome::RateLimited => RATE_LIMIT_ERROR.to_string(),
            SearchOutcome::HttpStatus(status) => {
                format!("❌ Error: API request failed with status {status}")
            }
            SearchOutcome::Timeout => TIMEOUT_ERROR.to_string(),
            SearchOutcome::Malformed(error) => unexpected_response_error(error),
            SearchOutcome::Network(details) => {
                format!("❌ Error: Network error while contacting grep.app API: {details}")
            }
        };

        Ok(text_result(output))
    }
}

impl ServerHandler for GrepServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
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
        let grep_query = Tool::new(
            "grep_query",
            "Search GitHub code using grep.app API. This tool enables AI assistants to search through GitHub repositories for specific code patterns using grep.app's powerful search index. It returns formatted results with repository information, file paths, and code snippets.",
            grep_query_schema(),
        )
        .annotate(
            ToolAnnotations::new()
                .read_only(true)
                .destructive(false)
                .idempotent(true)
                .open_world(true),
        );

        Ok(ListToolsResult {
            meta: None,
            tools: vec![grep_query],
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if request.name.as_ref() != "grep_query" {
            return Err(ErrorData::method_not_found::<CallToolRequestMethod>());
        }

        let params = match serde_json::from_value::<GrepQueryParams>(Value::Object(
            request.arguments.unwrap_or_default(),
        )) {
            Ok(params) => params,
            Err(_) => return Ok(text_result(QUERY_REQUIRED_ERROR)),
        };

        self.grep_query_impl(params).await
    }
}

fn unexpected_response_error(error: impl std::fmt::Display) -> String {
    format!("❌ Error: Unexpected response format from grep.app API: {error}")
}

fn text_result(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text.into())])
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
