//! MCP stdio adapter, independent of NATS journal/replay authority.
use super::invocation::{metric_tool_name, tool_exec_span};
use super::*;

#[derive(Clone)]
pub(super) struct McpToolsetAdapter {
    pub(super) toolset: Arc<dyn Toolset>,
}

impl ServerHandler for McpToolsetAdapter {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new(
                format!("harnx-{}-server", self.toolset.name()),
                env!("CARGO_PKG_VERSION"),
            ),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self
            .toolset
            .tools()
            .into_iter()
            .map(|spec| {
                let input_schema = match spec.input_schema {
                    Value::Object(schema) => schema,
                    _ => Map::new(),
                };
                let mut tool = Tool::new(spec.name, spec.description, input_schema).annotate(
                    ToolAnnotations::new()
                        .read_only(spec.read_only_hint)
                        .idempotent(spec.idempotent_hint),
                );
                tool.meta = spec.meta.map(MetaObject);
                tool
            })
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let parent_cx = harnx_telemetry::propagate::extract_context_from_mcp_meta(&context.meta);
        let span = tool_exec_span(&request.name, parent_cx);
        self.dispatch_call_tool(request, context)
            .instrument(span)
            .await
            .map(Into::into)
    }
}

impl McpToolsetAdapter {
    /// The tool dispatch, which always finishes in a single step.
    ///
    /// `call_tool` must return `CallToolResponse`, whose other variants cover
    /// elicitation and long-running tasks that this server does not use.
    /// Dispatching separately keeps every arm returning a plain
    /// `CallToolResult`.
    ///
    /// Tool dispatch forks: `run_toolset_main` has two mutually exclusive paths:
    /// NATS → `invoke_uncached_tool`, and MCP stdio/HTTP → this method (calls
    /// `toolset.invoke_with_context` directly). Any cross-cutting concern (metrics, tracing, auth)
    /// added at one seam does NOT automatically cover the other. Bespoke rmcp servers
    /// use their own `ServerHandler::call_tool`, a third seam.
    async fn dispatch_call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let tool_name = request.name.clone();
        if !self
            .toolset
            .tools()
            .iter()
            .any(|tool| tool.name == tool_name)
        {
            return Err(ErrorData::invalid_params(
                format!("unknown tool: {tool_name}"),
                None,
            ));
        }
        let args = Value::Object(request.arguments.unwrap_or_default());
        let capabilities = context
            .meta
            .contains_key(EXECUTION_CONTEXT_NAMESPACE)
            .then(|| EXECUTION_CONTEXT_NAMESPACE.to_string())
            .into_iter()
            .collect();
        let invocation_context = ToolInvocationContext {
            call_id: format!("{:?}", context.id),
            invoking_session_id: None,
            capabilities,
            // MCP transports have no journal to record a checkpoint in, and no
            // control subject a later cancel could arrive on.
            checkpoint: None,
            checkpoint_store: None,
        };
        let attestation = RequestAttestation {
            call_id: invocation_context.call_id.clone(),
            tool: tool_name.to_string(),
            capabilities: invocation_context.capabilities.clone(),
        };
        let metric_tool = metric_tool_name(self.toolset.as_ref(), &tool_name);
        let started = Instant::now();
        let mut result = self
            .toolset
            .invoke_with_context(ToolInvocation {
                tool: tool_name.to_string(),
                args,
                context: invocation_context.clone(),
                cancel: CancellationToken::new(),
            })
            .await;
        harnx_metrics::record_tool_call(metric_tool, result.is_ok(), started.elapsed());

        if let Ok(value) = &mut result {
            finalize_execution_context_value("mcp", self.toolset.name(), &attestation, value);
        }
        match result {
            Ok(value) => Ok(call_tool_result_from_value(value)),
            Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(
                error.to_string(),
            )])),
        }
    }
}

pub(super) fn call_tool_result_from_value(value: Value) -> CallToolResult {
    if let Ok(result) = serde_json::from_value::<CallToolResult>(value.clone()) {
        return result;
    }
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    CallToolResult::success(vec![ContentBlock::text(text)])
}
