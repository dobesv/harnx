use std::sync::OnceLock;

use fancy_regex::Regex;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
    Tool, ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};

use crate::client::{self, RequestOutcome};
use crate::format;

use super::model::{ContentsResponse, SearchResponse};
use super::{ExaServer, WebFetchParams, WebSearchParams};

const MISSING_KEY_ERROR: &str = "❌ Error: EXA_API_KEY is not set. Get a key at https://exa.ai and set it in ~/.local/share/harnx/.env";
const UNAUTHORIZED_ERROR: &str = "❌ Error: Invalid or unauthorized API key";
const RATE_LIMIT_ERROR: &str =
    "❌ Error: Exa API rate limit exceeded. Please wait before making another request.";
const TIMEOUT_ERROR: &str = "❌ Error: Request timed out while contacting the Exa API.";
const WEB_SEARCH_DESCRIPTION: &str =
    "Search the web with Exa and return titles, URLs, publication details, and highlights.";
const WEB_FETCH_DESCRIPTION: &str =
    "Fetch readable text from one or more URLs with Exa's content extraction API.";

fn metric_tool_name(tool: &str) -> &str {
    match tool {
        "web_search_exa" | "web_fetch_exa" => tool,
        _ => "unknown",
    }
}

fn tool_call_succeeded(result: &Result<CallToolResult, ErrorData>) -> bool {
    result
        .as_ref()
        .is_ok_and(|result| result.is_error != Some(true))
}

fn domain_result(result: Result<CallToolResult, ErrorData>) -> Result<CallToolResult, ErrorData> {
    match result {
        Ok(ok) => Ok(ok),
        Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(
            error.message,
        )])),
    }
}

fn text_result(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text.into())])
}

fn domain_error(message: impl Into<String>) -> ErrorData {
    ErrorData::internal_error(message.into(), None)
}

fn api_key() -> Result<String, ErrorData> {
    std::env::var("EXA_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| domain_error(MISSING_KEY_ERROR))
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

static CATEGORY_REGEX: OnceLock<Result<Regex, fancy_regex::Error>> = OnceLock::new();

pub(crate) fn extract_category(query: &str) -> Result<(String, Option<String>), ErrorData> {
    let regex = CATEGORY_REGEX
        .get_or_init(|| {
            Regex::new(r"(?i)\bcategory:(company|publication|news|personal\s*site|people)\b")
        })
        .as_ref()
        .map_err(|error| {
            domain_error(format!(
                "❌ Error: Failed to parse search category: {error}"
            ))
        })?;
    let captures = regex.captures(query).map_err(|error| {
        domain_error(format!(
            "❌ Error: Failed to parse search category: {error}"
        ))
    })?;
    let Some(captures) = captures else {
        return Ok((collapse_whitespace(query), None));
    };
    let whole = captures
        .get(0)
        .ok_or_else(|| domain_error("❌ Error: Failed to parse search category"))?;
    let category = captures
        .get(1)
        .map(|value| collapse_whitespace(&value.as_str().to_lowercase()))
        .ok_or_else(|| domain_error("❌ Error: Failed to parse search category"))?;
    let cleaned = format!("{} {}", &query[..whole.start()], &query[whole.end()..]);
    Ok((collapse_whitespace(&cleaned), Some(category)))
}

pub(crate) fn search_body(params: &WebSearchParams) -> Result<Value, ErrorData> {
    let (query, category) = extract_category(&params.query)?;
    let mut body = json!({
        "query": query,
        "type": "auto",
        "numResults": params.result_count(),
        "contents": { "highlights": true }
    });
    if let Some(category) = category {
        body.as_object_mut()
            .expect("search body is an object")
            .insert("category".to_owned(), Value::String(category));
    }
    Ok(body)
}

pub(crate) fn contents_body(params: &WebFetchParams) -> Value {
    json!({
        "urls": params.urls,
        "text": { "maxCharacters": params.character_limit() }
    })
}

fn outcome_value(outcome: RequestOutcome) -> Result<Value, ErrorData> {
    match outcome {
        RequestOutcome::Ok(value) => Ok(value),
        RequestOutcome::Unauthorized => Err(domain_error(UNAUTHORIZED_ERROR)),
        RequestOutcome::RateLimited => Err(domain_error(RATE_LIMIT_ERROR)),
        RequestOutcome::HttpStatus(status) => Err(domain_error(format!(
            "❌ Error: Exa API request failed with status {status}"
        ))),
        RequestOutcome::Timeout => Err(domain_error(TIMEOUT_ERROR)),
        RequestOutcome::Malformed(details) => Err(domain_error(format!(
            "❌ Error: Unexpected response format from Exa API: {details}"
        ))),
        RequestOutcome::Network(details) => Err(domain_error(format!(
            "❌ Error: Network error while contacting Exa API: {details}"
        ))),
    }
}

impl ExaServer {
    pub async fn web_search_impl(
        &self,
        params: WebSearchParams,
    ) -> Result<CallToolResult, ErrorData> {
        params.validate().map_err(domain_error)?;
        let key = api_key()?;
        let body = search_body(&params)?;
        let value = outcome_value(
            client::post_json(
                &self.client,
                &format!("{}/search", self.base_url),
                &key,
                "web-search-mcp",
                &body,
            )
            .await,
        )?;
        let response: SearchResponse = serde_json::from_value(value).map_err(|error| {
            domain_error(format!(
                "❌ Error: Unexpected response format from Exa API: {error}"
            ))
        })?;
        Ok(text_result(format::format_search(&response)))
    }

    pub async fn web_fetch_impl(
        &self,
        params: WebFetchParams,
    ) -> Result<CallToolResult, ErrorData> {
        params.validate().map_err(domain_error)?;
        let key = api_key()?;
        let body = contents_body(&params);
        let value = outcome_value(
            client::post_json(
                &self.client,
                &format!("{}/contents", self.base_url),
                &key,
                "crawling-mcp",
                &body,
            )
            .await,
        )?;
        let response: ContentsResponse = serde_json::from_value(value).map_err(|error| {
            domain_error(format!(
                "❌ Error: Unexpected response format from Exa API: {error}"
            ))
        })?;
        let errors = format::url_errors(&response);
        if response.results.is_empty() && !errors.is_empty() {
            return Err(domain_error(format::format_empty_contents_errors(&errors)));
        }
        Ok(text_result(format::format_contents(&response)))
    }
}

impl ServerHandler for ExaServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "harnx-exa-tools",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions("Search the web and fetch URL content through Exa.")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let annotations = || {
            ToolAnnotations::new()
                .read_only(true)
                .destructive(false)
                .idempotent(true)
                .open_world(true)
        };
        Ok(ListToolsResult::with_all_items(vec![
            Tool::new(
                "web_search_exa",
                WEB_SEARCH_DESCRIPTION,
                web_search_schema(),
            )
            .annotate(annotations()),
            Tool::new("web_fetch_exa", WEB_FETCH_DESCRIPTION, web_fetch_schema())
                .annotate(annotations()),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let tool = request.name.clone();
        let metric_tool = metric_tool_name(&tool);
        let start = std::time::Instant::now();
        let result = self.dispatch_call_tool(request, context).await;
        harnx_metrics::record_tool_call(metric_tool, tool_call_succeeded(&result), start.elapsed());
        result.map(Into::into)
    }
}

impl ExaServer {
    async fn dispatch_call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        match request.name.as_ref() {
            "web_search_exa" => domain_result(
                async {
                    let params = parse_arguments::<WebSearchParams>(request.arguments)?;
                    self.web_search_impl(params).await
                }
                .await,
            ),
            "web_fetch_exa" => domain_result(
                async {
                    let params = parse_arguments::<WebFetchParams>(request.arguments)?;
                    self.web_fetch_impl(params).await
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

fn parse_arguments<T: DeserializeOwned>(
    arguments: Option<Map<String, Value>>,
) -> Result<T, ErrorData> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default())).map_err(|error| {
        ErrorData::invalid_params(format!("invalid tool arguments: {error}"), None)
    })
}

pub(crate) fn web_search_schema() -> Map<String, Value> {
    let mut properties = Map::new();
    properties.insert(
        "query".to_owned(),
        string_property("Natural language search query. Optionally include category:(company|publication|news|personal site|people) to focus results."),
    );
    properties.insert(
        "numResults".to_owned(),
        json!({
            "type": "integer",
            "description": "Number of search results to return.",
            "default": 10
        }),
    );
    object_schema(properties, &["query"])
}

pub(crate) fn web_fetch_schema() -> Map<String, Value> {
    let mut properties = Map::new();
    properties.insert(
        "urls".to_owned(),
        json!({
            "type": "array",
            "items": { "type": "string" },
            "description": "URLs to read. Batch multiple URLs in one call."
        }),
    );
    properties.insert(
        "maxCharacters".to_owned(),
        json!({
            "type": "integer",
            "description": "Maximum number of characters to extract from each URL.",
            "default": 3000,
            "minimum": 1
        }),
    );
    object_schema(properties, &["urls"])
}

fn object_schema(properties: Map<String, Value>, required: &[&str]) -> Map<String, Value> {
    let mut schema = Map::new();
    schema.insert("type".to_owned(), Value::String("object".to_owned()));
    schema.insert("properties".to_owned(), Value::Object(properties));
    schema.insert(
        "required".to_owned(),
        Value::Array(
            required
                .iter()
                .map(|name| Value::String((*name).to_owned()))
                .collect(),
        ),
    );
    schema
}

fn string_property(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_category_case_insensitively_and_collapses_whitespace() {
        assert_eq!(
            extract_category("  latest   AI category:Personal   Site updates ").unwrap(),
            (
                "latest AI updates".to_owned(),
                Some("personal site".to_owned())
            )
        );
        assert_eq!(
            extract_category("category:NEWS today").unwrap(),
            ("today".to_owned(), Some("news".to_owned()))
        );
        assert_eq!(
            extract_category("company research").unwrap(),
            ("company research".to_owned(), None)
        );
    }

    #[test]
    fn builds_exact_search_body_with_and_without_category() {
        let params = WebSearchParams {
            query: "rust category:company async".into(),
            num_results: Some(4),
        };
        assert_eq!(
            search_body(&params).unwrap(),
            json!({
                "query": "rust async",
                "type": "auto",
                "numResults": 4,
                "contents": {"highlights": true},
                "category": "company"
            })
        );
        let params = WebSearchParams {
            query: "rust async".into(),
            num_results: None,
        };
        assert_eq!(search_body(&params).unwrap()["numResults"], 10);
        assert!(search_body(&params).unwrap().get("category").is_none());
    }

    #[test]
    fn builds_contents_options_at_top_level() {
        let body = contents_body(&WebFetchParams {
            urls: vec!["https://example.com".into()],
            max_characters: None,
        });
        assert_eq!(
            body,
            json!({
                "urls": ["https://example.com"],
                "text": {"maxCharacters": 3000}
            })
        );
        assert!(body.get("contents").is_none());
    }

    #[test]
    fn metrics_use_bounded_tool_names() {
        assert_eq!(metric_tool_name("web_search_exa"), "web_search_exa");
        assert_eq!(metric_tool_name("web_fetch_exa"), "web_fetch_exa");
        assert_eq!(metric_tool_name("anything"), "unknown");
    }

    #[test]
    fn schemas_match_primary_contract() {
        let search = Value::Object(web_search_schema());
        assert_eq!(search["required"], json!(["query"]));
        assert_eq!(search["properties"]["numResults"]["default"], 10);
        let fetch = Value::Object(web_fetch_schema());
        assert_eq!(fetch["required"], json!(["urls"]));
        assert_eq!(fetch["properties"]["urls"]["type"], "array");
        assert_eq!(fetch["properties"]["maxCharacters"]["minimum"], 1);
    }
}
