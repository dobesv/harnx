//! Deterministic, script-driven mock MCP server.
//!
//! Advertises a fixed set of tools (defined in a YAML script) and returns
//! canned results for each `tools/call` in order. Used to fake a realistic
//! agent coding session in demo recordings without running any real tools.
//!
//! # Script format
//!
//! ```yaml
//! tools:
//!   - name: read_file
//!     description: Read a file from the project.
//!     call_template: "📄 read {{ args.path }}"
//!   - name: run
//!     description: Run a shell command.
//!     call_template: "```sh\n$ {{ args.command }}\n```"
//! responses:
//!   - "pub fn add(a: i64, b: i64) -> i64 { a + b }"
//!   - "running 1 test\ntest result: ok. 1 passed"
//! fallback: "(no more scripted responses)"
//! ```
//!
//! Tool calls consume `responses` in order (FIFO). Once exhausted, `fallback`
//! is returned. `call_template`, when present, is emitted in the tool's `_meta`
//! so the harnx TUI renders the call nicely (the same mechanism the real bash
//! MCP server uses).

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    Implementation, ListToolsResult, MetaObject, PaginatedRequestParams, ServerCapabilities,
    ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicUsize, Ordering};

/// One tool advertised by the mock server.
#[derive(Debug, Clone, Deserialize)]
pub struct MockToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// MiniJinja template the TUI uses to render the tool call (via `_meta`).
    #[serde(default)]
    pub call_template: Option<String>,
}

/// Parsed mock-MCP script.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MockMcpScript {
    #[serde(default)]
    pub tools: Vec<MockToolDef>,
    /// Canned tool results, consumed in order across all `tools/call` requests.
    #[serde(default)]
    pub responses: Vec<String>,
    /// Returned once `responses` is exhausted.
    #[serde(default)]
    pub fallback: Option<String>,
    /// Artificial delay before each tool result, to pace a recording so it
    /// reads like real work (reading, editing, running tests) rather than an
    /// instant blip. Defaults to 0 (no delay).
    #[serde(default)]
    pub response_delay_ms: u64,
}

pub struct MockMcpServer {
    script: MockMcpScript,
    cursor: AtomicUsize,
}

impl MockMcpServer {
    /// Build a server from a YAML script string.
    pub fn from_script_str(yaml: &str) -> anyhow::Result<Self> {
        let script: MockMcpScript = serde_yaml::from_str(yaml)?;
        Ok(Self::from_script(script))
    }

    pub fn from_script(script: MockMcpScript) -> Self {
        Self {
            script,
            cursor: AtomicUsize::new(0),
        }
    }

    pub fn tool_defs(&self) -> &[MockToolDef] {
        &self.script.tools
    }

    pub fn response_delay(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.script.response_delay_ms)
    }

    /// Resolve the next canned response for a call to `tool_name`.
    ///
    /// Returns `Err` with a message if the tool is not advertised. Otherwise
    /// returns the next unconsumed response, or the fallback text once the
    /// scripted responses are exhausted.
    pub fn next_response(&self, tool_name: &str) -> Result<String, String> {
        if !self.script.tools.iter().any(|t| t.name == tool_name) {
            return Err(format!("unknown tool: {tool_name}"));
        }
        let idx = self.cursor.fetch_add(1, Ordering::SeqCst);
        let response = self.script.responses.get(idx).cloned().unwrap_or_else(|| {
            self.script
                .fallback
                .clone()
                .unwrap_or_else(|| "(no more scripted responses)".to_string())
        });
        Ok(response)
    }

    fn build_tools(&self) -> Vec<Tool> {
        // Permissive schema: the mock LLM supplies whatever arguments the
        // call_template references (path, command, …); accept them all.
        let input_schema = Map::from_iter([
            ("type".to_string(), Value::String("object".to_string())),
            ("properties".to_string(), Value::Object(Map::new())),
            ("additionalProperties".to_string(), Value::Bool(true)),
        ]);

        self.tool_defs()
            .iter()
            .map(|def| {
                let mut tool = Tool::new(
                    def.name.clone(),
                    def.description.clone(),
                    input_schema.clone(),
                );
                if let Some(template) = &def.call_template {
                    let meta = json!({ "call_template": template })
                        .as_object()
                        .expect("object literal")
                        .clone();
                    tool = tool.with_meta(MetaObject(meta));
                }
                tool
            })
            .collect()
    }
}

impl ServerHandler for MockMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "harnx-mock-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions("Deterministic, script-driven mock MCP server for demo recordings.")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.build_tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.dispatch_call_tool(request, _context)
            .await
            .map(Into::into)
    }
}

impl MockMcpServer {
    /// The tool dispatch, which always finishes in a single step.
    ///
    /// `call_tool` must return `CallToolResponse`, whose other variants cover
    /// elicitation and long-running tasks that this server does not use.
    /// Dispatching separately keeps every arm returning a plain
    /// `CallToolResult`.
    async fn dispatch_call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        // Resolve the result first so an unknown tool errors without delay.
        let text = match self.next_response(request.name.as_ref()) {
            Ok(text) => text,
            Err(msg) => return Err(ErrorData::invalid_params(msg, None)),
        };
        let delay = self.response_delay();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str = r#"
tools:
  - name: read_file
    description: Read a file.
    call_template: "read {{ args.path }}"
  - name: run
    description: Run a command.
responses:
  - "file contents"
  - "command output"
fallback: "(done)"
"#;

    #[test]
    fn parses_tools_and_responses() {
        let server = MockMcpServer::from_script_str(SCRIPT).unwrap();
        let defs = server.tool_defs();
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "read_file");
        assert_eq!(
            defs[0].call_template.as_deref(),
            Some("read {{ args.path }}")
        );
        assert_eq!(defs[1].name, "run");
        assert_eq!(defs[1].call_template, None);
    }

    #[test]
    fn returns_responses_in_order_then_fallback() {
        let server = MockMcpServer::from_script_str(SCRIPT).unwrap();
        assert_eq!(server.next_response("read_file").unwrap(), "file contents");
        assert_eq!(server.next_response("run").unwrap(), "command output");
        // Exhausted -> fallback.
        assert_eq!(server.next_response("run").unwrap(), "(done)");
    }

    #[test]
    fn unknown_tool_is_an_error() {
        let server = MockMcpServer::from_script_str(SCRIPT).unwrap();
        let err = server.next_response("nope").unwrap_err();
        assert!(err.contains("unknown tool"), "got: {err}");
    }

    #[test]
    fn default_fallback_when_unspecified() {
        let server =
            MockMcpServer::from_script_str("tools:\n  - name: t\nresponses: []\n").unwrap();
        assert_eq!(
            server.next_response("t").unwrap(),
            "(no more scripted responses)"
        );
    }

    #[test]
    fn response_delay_defaults_to_zero_and_parses() {
        let server = MockMcpServer::from_script_str(SCRIPT).unwrap();
        assert!(server.response_delay().is_zero());

        let delayed = MockMcpServer::from_script_str(
            "tools:\n  - name: t\nresponses: []\nresponse_delay_ms: 250\n",
        )
        .unwrap();
        assert_eq!(delayed.response_delay().as_millis(), 250);
    }

    #[test]
    fn builds_tools_with_call_template_meta() {
        let server = MockMcpServer::from_script_str(SCRIPT).unwrap();
        let tools = server.build_tools();
        assert_eq!(tools.len(), 2);
        // read_file has a call_template -> _meta set; run does not.
        assert!(tools[0].meta.is_some());
        assert!(tools[1].meta.is_none());
    }
}
