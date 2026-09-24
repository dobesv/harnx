use async_trait::async_trait;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
use rmcp::model::{CallToolResult, ErrorData};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::server::{
    tool_schema, FetchParams, FetchServer, YoutubeTranscriptParams, TOOL_DESCRIPTIONS,
};

#[derive(Clone)]
pub struct FetchToolset {
    server: FetchServer,
}

impl FetchToolset {
    pub fn new() -> Self {
        let allow_private_ip = std::env::args().any(|argument| argument == "--allow-private-ip");
        Self::with_allow_private_ip(allow_private_ip)
    }

    pub fn with_allow_private_ip(allow_private_ip: bool) -> Self {
        Self {
            server: FetchServer::new(allow_private_ip),
        }
    }

    pub fn server(&self) -> &FetchServer {
        &self.server
    }
}

impl Default for FetchToolset {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_args<T: DeserializeOwned>(args: Value) -> Result<T, ToolInvokeError> {
    serde_json::from_value(args)
        .map_err(|error| ToolInvokeError::Recoverable(format!("invalid tool arguments: {error}")))
}

fn map_result(result: Result<CallToolResult, ErrorData>) -> Result<Value, ToolInvokeError> {
    match result {
        Ok(result) => serde_json::to_value(result).map_err(|error| {
            ToolInvokeError::Fatal(format!("failed to serialize tool result: {error}"))
        }),
        Err(error) => Err(ToolInvokeError::Recoverable(error.message.to_string())),
    }
}

fn tool_spec(name: &str, description: &str) -> ToolSpec {
    ToolSpec {
        cancellation_guarantee: Default::default(),
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema: Value::Object(tool_schema(name)),
        idempotent_hint: true,
        read_only_hint: true,
        timeout_secs: Some(35),
        meta: None,
    }
}

#[async_trait]
impl Toolset for FetchToolset {
    fn name(&self) -> &str {
        "fetch"
    }

    fn default_mcp_http_port(&self) -> u16 {
        3006
    }

    fn tools(&self) -> Vec<ToolSpec> {
        TOOL_DESCRIPTIONS
            .iter()
            .map(|(name, description)| tool_spec(name, description))
            .collect()
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        match tool {
            "fetch_html" => map_result(
                self.server
                    .fetch_html_impl(parse_args::<FetchParams>(args)?)
                    .await,
            ),
            "fetch_markdown" => map_result(
                self.server
                    .fetch_markdown_impl(parse_args::<FetchParams>(args)?)
                    .await,
            ),
            "fetch_txt" => map_result(
                self.server
                    .fetch_txt_impl(parse_args::<FetchParams>(args)?)
                    .await,
            ),
            "fetch_json" => map_result(
                self.server
                    .fetch_json_impl(parse_args::<FetchParams>(args)?)
                    .await,
            ),
            "fetch_readable" => map_result(
                self.server
                    .fetch_readable_impl(parse_args::<FetchParams>(args)?)
                    .await,
            ),
            "fetch_youtube_transcript" => map_result(
                self.server
                    .fetch_youtube_transcript_impl(parse_args::<YoutubeTranscriptParams>(args)?)
                    .await,
            ),
            _ => Err(ToolInvokeError::Recoverable(format!(
                "unknown fetch tool: {tool}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exposes_exact_drop_in_tool_specs() {
        let toolset = FetchToolset::with_allow_private_ip(false);
        assert_eq!(toolset.name(), "fetch");
        assert_eq!(toolset.default_mcp_http_port(), 3006);
        let tools = toolset.tools();
        assert_eq!(tools.len(), 6);
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            [
                "fetch_html",
                "fetch_markdown",
                "fetch_txt",
                "fetch_json",
                "fetch_readable",
                "fetch_youtube_transcript"
            ]
        );
        assert!(tools.iter().all(|tool| {
            tool.input_schema["required"] == json!(["url"])
                && tool.idempotent_hint
                && tool.read_only_hint
        }));
    }

    #[tokio::test]
    async fn unknown_tool_is_recoverable() {
        let result = FetchToolset::with_allow_private_ip(false)
            .invoke("missing", json!({}), CancellationToken::new())
            .await;
        assert!(matches!(result, Err(ToolInvokeError::Recoverable(_))));
    }
}
