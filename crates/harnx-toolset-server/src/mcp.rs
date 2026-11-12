//! MCP stdio adapter, independent of NATS journal/replay authority.
use super::invocation::{metric_tool_name, tool_exec_span};
use super::*;

#[derive(Clone)]
pub(super) struct McpToolsetAdapter {
    pub(super) toolset: Arc<dyn Toolset>,
}

impl ServerHandler for McpToolsetAdapter {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
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
                // MCP tool listings have no timeout field, so the request-timeout
                // policy rides in `_meta`; `harnx-mcp-bridge` copies it back into
                // `ToolSpec.timeout_secs` for NATS callers. Without it, a bridged
                // long-running tool (bash spawn's nested wait) would inherit the
                // caller's request backstop instead of the server's policy.
                let mut meta = spec.meta.unwrap_or_default();
                // MCP has no control subject to deliver a new-message signal
                // on, so a tool reached this way cannot stop on one, and a
                // bridge must not advertise that it does.
                meta.remove(harnx_toolset::STOP_ON_NEW_MESSAGE_META_KEY);
                if let Some(timeout_secs) = spec.timeout_secs {
                    meta.insert(
                        harnx_toolset::TOOL_TIMEOUT_META_KEY.to_owned(),
                        Value::from(timeout_secs),
                    );
                }
                tool.meta = (!meta.is_empty()).then_some(MetaObject(meta));
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
            run_context: None,
            call_id: format!("{:?}", context.id),
            invoking_session_id: None,
            invoking_session: None,
            capabilities,
            // MCP transports have no journal to record a checkpoint or a
            // partial result in, and no control subject a later cancel or
            // new-message signal could arrive on.
            checkpoint: None,
            checkpoint_store: None,
            partial_result_store: None,
            progress: Default::default(),
            new_message: Default::default(),
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
