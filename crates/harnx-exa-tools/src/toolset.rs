use crate::server::{
    web_fetch_schema, web_search_schema, ExaServer, WebFetchParams, WebSearchParams,
};
use async_trait::async_trait;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
use rmcp::model::{CallToolResult, ErrorData};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

const WEB_SEARCH_DESCRIPTION: &str =
    "Search the web with Exa and return titles, URLs, publication details, and highlights.";
const WEB_FETCH_DESCRIPTION: &str =
    "Fetch readable text from one or more URLs with Exa's content extraction API.";

/// Native toolset for Exa-backed web search and content fetching.
#[derive(Clone)]
pub struct ExaToolset {
    server: ExaServer,
}

impl ExaToolset {
    pub fn new() -> Self {
        Self {
            server: ExaServer::new(),
        }
    }

    /// Creates a toolset targeting a custom Exa-compatible API endpoint.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            server: ExaServer::with_base_url(base_url),
        }
    }
}

impl Default for ExaToolset {
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

fn tool_spec(
    name: &str,
    description: &str,
    input_schema: serde_json::Map<String, Value>,
) -> ToolSpec {
    ToolSpec {
        cancellation_guarantee: Default::default(),
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema: Value::Object(input_schema),
        idempotent_hint: true,
        read_only_hint: true,
        timeout_secs: None,
        meta: None,
    }
}

#[async_trait]
impl Toolset for ExaToolset {
    fn name(&self) -> &str {
        "exa"
    }

    fn default_mcp_http_port(&self) -> u16 {
        3005
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![
            tool_spec(
                "web_search_exa",
                WEB_SEARCH_DESCRIPTION,
                web_search_schema(),
            ),
            tool_spec("web_fetch_exa", WEB_FETCH_DESCRIPTION, web_fetch_schema()),
        ]
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        match tool {
            "web_search_exa" => {
                let params = parse_args::<WebSearchParams>(args)?;
                map_result(self.server.web_search_impl(params).await)
            }
            "web_fetch_exa" => {
                let params = parse_args::<WebFetchParams>(args)?;
                map_result(self.server.web_fetch_impl(params).await)
            }
            _ => Err(ToolInvokeError::Recoverable(format!(
                "unknown exa tool: {tool}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_toolset::Toolset;
    use serde_json::json;

    #[test]
    fn exposes_exact_drop_in_tool_specs() {
        let toolset = ExaToolset::new();
        assert_eq!(toolset.name(), "exa");
        assert_eq!(toolset.default_mcp_http_port(), 3005);
        let tools = toolset.tools();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "web_search_exa");
        assert_eq!(tools[1].name, "web_fetch_exa");
        assert_eq!(tools[0].input_schema["required"], json!(["query"]));
        assert_eq!(tools[1].input_schema["required"], json!(["urls"]));
        assert!(tools
            .iter()
            .all(|tool| tool.idempotent_hint && tool.read_only_hint));
    }

    #[test]
    fn maps_all_server_errors_to_recoverable() {
        for error in [
            ErrorData::internal_error("server failed", None),
            ErrorData::invalid_params("bad input", None),
        ] {
            assert!(matches!(
                map_result(Err(error)),
                Err(ToolInvokeError::Recoverable(_))
            ));
        }
    }

    #[tokio::test]
    async fn missing_or_empty_key_is_recoverable_for_native_calls() {
        let old = std::env::var_os("EXA_API_KEY");
        // SAFETY: nextest runs each test in a separate process, and this is the only
        // unit test in this process that changes EXA_API_KEY.
        for key in [None, Some("")] {
            match key {
                Some(value) => unsafe { std::env::set_var("EXA_API_KEY", value) },
                None => unsafe { std::env::remove_var("EXA_API_KEY") },
            }
            let result = ExaToolset::new()
                .invoke(
                    "web_search_exa",
                    json!({"query": "rust"}),
                    CancellationToken::new(),
                )
                .await;
            assert!(matches!(
                result,
                Err(ToolInvokeError::Recoverable(message))
                    if message == "❌ Error: EXA_API_KEY is not set. Get a key at https://exa.ai and set it in ~/.local/share/harnx/.env"
            ));
        }
        match old {
            Some(value) => unsafe { std::env::set_var("EXA_API_KEY", value) },
            None => unsafe { std::env::remove_var("EXA_API_KEY") },
        }
    }

    #[tokio::test]
    async fn rejects_unknown_tool() {
        let result = ExaToolset::new()
            .invoke("missing", json!({}), CancellationToken::new())
            .await;
        assert!(matches!(result, Err(ToolInvokeError::Recoverable(_))));
    }
}
