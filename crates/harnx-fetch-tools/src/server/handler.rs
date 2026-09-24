use harnx_core::safety::truncate_output;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
    Tool, ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};

use crate::format;

use super::model::PlayerResponse;
use super::{FetchParams, FetchServer, YoutubeTranscriptParams};

pub(crate) const TOOL_DESCRIPTIONS: [(&str, &str); 6] = [
    ("fetch_html", "Fetch a URL and return its raw HTML."),
    (
        "fetch_markdown",
        "Fetch a URL and convert its HTML content to Markdown.",
    ),
    (
        "fetch_txt",
        "Fetch a URL and return plain DOM text with scripts and styles removed.",
    ),
    (
        "fetch_json",
        "Fetch a URL, validate its JSON response, and return pretty-printed JSON.",
    ),
    (
        "fetch_readable",
        "Fetch a URL and extract its main readable article as Markdown.",
    ),
    (
        "fetch_youtube_transcript",
        "Fetch a YouTube watch page and return its timestamped caption transcript.",
    ),
];

fn domain_error(message: impl Into<String>) -> ErrorData {
    ErrorData::internal_error(format!("❌ Error: {}", message.into()), None)
}

fn text_result(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text.into())])
}

fn domain_result(result: Result<CallToolResult, ErrorData>) -> Result<CallToolResult, ErrorData> {
    match result {
        Ok(ok) => Ok(ok),
        Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(
            error.message,
        )])),
    }
}

fn tool_call_succeeded(result: &Result<CallToolResult, ErrorData>) -> bool {
    result
        .as_ref()
        .is_ok_and(|result| result.is_error != Some(true))
}

fn metric_tool_name(tool: &str) -> &'static str {
    TOOL_DESCRIPTIONS
        .iter()
        .find_map(|(name, _)| (*name == tool).then_some(*name))
        .unwrap_or("unknown")
}

fn output(value: &str, params: &FetchParams, disable_line_clipping: bool) -> String {
    let (window, _) = params.compatibility_window(value);
    truncate_output(&window, &params.truncation_opts(disable_line_clipping))
}

fn json_output(value: &str, params: &FetchParams) -> String {
    let (window, compatibility_truncated) = params.compatibility_window(value);
    let preview = if compatibility_truncated {
        format!("[JSON preview: output truncated by max_length/start_index]\n{window}")
    } else {
        window
    };
    truncate_output(&preview, &params.truncation_opts(true))
}

impl FetchServer {
    async fn fetch_page(
        &self,
        params: &FetchParams,
    ) -> Result<crate::client::FetchedPage, ErrorData> {
        params.validate().map_err(domain_error)?;
        self.client
            .fetch(&params.url, &params.headers, params.proxy.as_deref())
            .await
            .map_err(domain_error)
    }

    pub async fn fetch_html_impl(&self, params: FetchParams) -> Result<CallToolResult, ErrorData> {
        let page = self.fetch_page(&params).await?;
        Ok(text_result(output(&page.body, &params, true)))
    }

    pub async fn fetch_markdown_impl(
        &self,
        params: FetchParams,
    ) -> Result<CallToolResult, ErrorData> {
        let page = self.fetch_page(&params).await?;
        Ok(text_result(output(
            &format::html_to_md(&page.body),
            &params,
            false,
        )))
    }

    pub async fn fetch_txt_impl(&self, params: FetchParams) -> Result<CallToolResult, ErrorData> {
        let page = self.fetch_page(&params).await?;
        let text = format::html_to_text(&page.body).map_err(domain_error)?;
        Ok(text_result(output(&text, &params, false)))
    }

    pub async fn fetch_json_impl(&self, params: FetchParams) -> Result<CallToolResult, ErrorData> {
        let page = self.fetch_page(&params).await?;
        let parsed: Value = serde_json::from_str(&page.body)
            .map_err(|error| domain_error(format!("response is not valid JSON: {error}")))?;
        let pretty = serde_json::to_string_pretty(&parsed)
            .map_err(|error| domain_error(format!("failed to serialize JSON: {error}")))?;
        Ok(text_result(json_output(&pretty, &params)))
    }

    pub async fn fetch_readable_impl(
        &self,
        params: FetchParams,
    ) -> Result<CallToolResult, ErrorData> {
        let page = self.fetch_page(&params).await?;
        let markdown =
            format::readable_markdown(&page.body, page.final_url.as_str()).map_err(domain_error)?;
        Ok(text_result(output(&markdown, &params, false)))
    }

    pub async fn fetch_youtube_transcript_impl(
        &self,
        params: YoutubeTranscriptParams,
    ) -> Result<CallToolResult, ErrorData> {
        params.validate().map_err(domain_error)?;
        let watch_page = self.fetch_page(&params.fetch).await?;
        let player_json = format::player_response_json(&watch_page.body).map_err(domain_error)?;
        let player: PlayerResponse = serde_json::from_str(player_json)
            .map_err(|error| domain_error(format!("invalid YouTube player response: {error}")))?;
        let tracks = player
            .captions
            .ok_or_else(|| domain_error("YouTube video has no captions"))?
            .player_captions_tracklist_renderer
            .caption_tracks;
        let track = tracks
            .iter()
            .find(|track| track.language_code.eq_ignore_ascii_case(params.lang.trim()))
            .ok_or_else(|| {
                let languages = tracks
                    .iter()
                    .map(|track| track.language_code.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                domain_error(format!(
                    "no YouTube caption track for language '{}'; available: {}",
                    params.lang,
                    if languages.is_empty() {
                        "none"
                    } else {
                        &languages
                    }
                ))
            })?;
        let mut captions_url = reqwest::Url::parse(&track.base_url)
            .map_err(|error| domain_error(format!("invalid YouTube caption URL: {error}")))?;
        captions_url.query_pairs_mut().append_pair("fmt", "srv1");
        let captions = self
            .client
            .fetch(
                captions_url.as_str(),
                &params.fetch.headers,
                params.fetch.proxy.as_deref(),
            )
            .await
            .map_err(domain_error)?;
        let transcript = format::transcript_lines(&captions.body).map_err(domain_error)?;
        Ok(text_result(output(&transcript, &params.fetch, false)))
    }
}

impl ServerHandler for FetchServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "harnx-fetch-tools",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions("Fetch and transform public HTTP and HTTPS resources.")
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
        Ok(ListToolsResult::with_all_items(
            TOOL_DESCRIPTIONS
                .iter()
                .map(|(name, description)| {
                    Tool::new(*name, *description, tool_schema(name)).annotate(annotations())
                })
                .collect(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let metric_tool = metric_tool_name(&request.name);
        let start = std::time::Instant::now();
        let result = self.dispatch_call_tool(request, context).await;
        harnx_metrics::record_tool_call(metric_tool, tool_call_succeeded(&result), start.elapsed());
        result.map(Into::into)
    }
}

impl FetchServer {
    async fn dispatch_call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let arguments = request.arguments;
        match request.name.as_ref() {
            "fetch_html" => domain_result(self.fetch_html_impl(parse_arguments(arguments)?).await),
            "fetch_markdown" => {
                domain_result(self.fetch_markdown_impl(parse_arguments(arguments)?).await)
            }
            "fetch_txt" => domain_result(self.fetch_txt_impl(parse_arguments(arguments)?).await),
            "fetch_json" => domain_result(self.fetch_json_impl(parse_arguments(arguments)?).await),
            "fetch_readable" => {
                domain_result(self.fetch_readable_impl(parse_arguments(arguments)?).await)
            }
            "fetch_youtube_transcript" => domain_result(
                self.fetch_youtube_transcript_impl(parse_arguments(arguments)?)
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

pub(crate) fn tool_schema(name: &str) -> Map<String, Value> {
    let mut properties = Map::new();
    properties.insert(
        "url".to_owned(),
        json!({"type":"string","description":"HTTP or HTTPS URL to fetch."}),
    );
    properties.insert(
        "headers".to_owned(),
        json!({"type":"object","additionalProperties":{"type":"string"},"description":"Request headers as string values."}),
    );
    properties.insert(
        "proxy".to_owned(),
        json!({"type":"string","description":"Proxy URL. Rejected unless server started with --allow-private-ip."}),
    );
    properties.insert(
        "max_length".to_owned(),
        json!({"type":"integer","minimum":0,"default":5000,"description":"Compatibility character limit applied before smart truncation. Zero means unlimited."}),
    );
    properties.insert(
        "start_index".to_owned(),
        json!({"type":"integer","minimum":0,"default":0,"description":"Compatibility character offset applied before smart truncation."}),
    );
    properties.insert(
        "head_lines".to_owned(),
        json!({"type":"integer","minimum":0,"description":"Lines to preserve from the start."}),
    );
    properties.insert(
        "tail_lines".to_owned(),
        json!({"type":"integer","minimum":0,"description":"Lines to preserve from the end."}),
    );
    properties.insert(
        "max_output_bytes".to_owned(),
        json!({"type":"integer","minimum":1,"description":"Maximum output size after transformation."}),
    );
    if name == "fetch_youtube_transcript" {
        properties.insert(
            "lang".to_owned(),
            json!({"type":"string","default":"en","description":"YouTube caption language code."}),
        );
    }
    let mut schema = Map::new();
    schema.insert("type".to_owned(), Value::String("object".to_owned()));
    schema.insert("properties".to_owned(), Value::Object(properties));
    schema.insert("required".to_owned(), json!(["url"]));
    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_schemas_require_url_and_expose_truncation() {
        for (name, _) in TOOL_DESCRIPTIONS {
            let schema = Value::Object(tool_schema(name));
            assert_eq!(schema["required"], json!(["url"]));
            assert_eq!(schema["properties"]["max_length"]["default"], 5000);
            assert!(schema["properties"].get("max_output_bytes").is_some());
        }
    }
}
