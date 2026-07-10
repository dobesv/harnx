use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde_json::{Map, Value};
use std::sync::OnceLock;

const TOOL_NAME: &str = "repro249_unique_mcp_tool";
const TOOL_RESPONSE: &str = "repro249 fixed tool response";

fn process_pid() -> &'static str {
    static PROCESS_PID: OnceLock<String> = OnceLock::new();
    PROCESS_PID
        .get_or_init(|| std::process::id().to_string())
        .as_str()
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Repro249Server;

impl ServerHandler for Repro249Server {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "harnx-mcp-repro249",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions("Deterministic fake MCP server for the repro_249 tmux test.")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let input_schema = Map::from_iter([
            ("type".to_string(), Value::String("object".to_string())),
            ("properties".to_string(), Value::Object(Map::new())),
            ("additionalProperties".to_string(), Value::Bool(false)),
        ]);

        Ok(ListToolsResult {
            meta: None,
            tools: vec![Tool::new(
                TOOL_NAME,
                "Deterministic fake MCP tool for the repro_249 tmux test.",
                input_schema,
            )],
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        match request.name.as_ref() {
            TOOL_NAME => Ok(CallToolResult::success(vec![Content::text(format!(
                "{TOOL_RESPONSE}\nPROCESS_PID={}",
                process_pid()
            ))])),
            other => Err(ErrorData::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }
}
