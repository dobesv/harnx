//! MCP stdio adapter, independent of NATS journal/replay authority.
use super::invocation::{metric_tool_name, tool_exec_span};
use super::*;

/// Serves a harnx toolset over MCP (`ServerHandler`), independent of NATS
/// journal/replay authority.
///
/// Public for in-process MCP hops: `run_mcp_stdio` builds it for the
/// `--mcp-stdio` entry point, and tests pair it with an in-memory rmcp
/// transport instead of spawning a child process.
#[derive(Clone)]
pub struct McpToolsetAdapter {
    pub toolset: Arc<dyn Toolset>,
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
    }
}

impl McpToolsetAdapter {
    /// The tool dispatch, which finishes in a single step or an MRTR
    /// `input_required` round trip.
    ///
    /// `call_tool` must return `CallToolResponse`; a tool that challenges
    /// (`ToolInvokeError::InputRequired`) becomes the `InputRequired` variant
    /// for MRTR-capable peers, and every other arm still returns a plain
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
    ) -> Result<CallToolResponse, ErrorData> {
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
        // rmcp hard-rejects an `input_required` result for a peer that
        // negotiated a pre-MRTR protocol version, which would surface as a
        // protocol error. Degrade to the visible compatibility error instead,
        // exactly as older harnx callers already see over the tool wire.
        let mrtr_capable = negotiated_version(&context)
            .as_ref()
            .is_some_and(|version| version >= &rmcp::model::ProtocolVersion::V_2026_07_28);
        let args = Value::Object(request.arguments.clone().unwrap_or_default());
        let invocation_context = mcp_invocation_context(&request, &context)?;
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
        Ok(call_tool_response(result, mrtr_capable))
    }
}

/// Map the tool's outcome onto an MCP `CallToolResponse`: a challenge becomes
/// the MRTR `InputRequired` variant for MRTR-capable peers, and degrades to
/// the visible compatibility error for peers that negotiated an older
/// protocol version. A challenge that cannot be mapped degrades to a visible
/// error result, never a silent success.
fn call_tool_response(
    result: Result<Value, ToolInvokeError>,
    mrtr_capable: bool,
) -> CallToolResponse {
    match result {
        Ok(value) => call_tool_result_from_value(value).into(),
        Err(ToolInvokeError::InputRequired(input)) => {
            if !mrtr_capable {
                return CallToolResult::error(vec![ContentBlock::text(
                    harnx_toolset::INPUT_REQUIRED_COMPAT_ERROR.to_string(),
                )])
                .into();
            }
            // The tool cannot finish without the user: surface the MRTR
            // input_required result the client re-calls with
            // inputResponses/requestState for.
            match crate::mrtr::challenge_to_mrtr(&input) {
                Ok(mrtr) => mrtr.into(),
                Err(error) => CallToolResult::error(vec![ContentBlock::text(error)]).into(),
            }
        }
        Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]).into(),
    }
}

fn negotiated_version(
    context: &RequestContext<RoleServer>,
) -> Option<rmcp::model::ProtocolVersion> {
    context.protocol_version()
}

/// MCP transports have no journal to record a checkpoint or a partial result
/// in, and no control subject a later cancel or new-message signal could
/// arrive on. Runtime-owned challenge answers and the echoed request state
/// arrive beside `arguments` on the re-call (MRTR `inputResponses` /
/// `requestState`), never inside them; anything undecodable is an
/// `invalid_params` error naming the entry.
fn mcp_invocation_context(
    request: &CallToolRequestParams,
    context: &RequestContext<RoleServer>,
) -> Result<ToolInvocationContext, ErrorData> {
    let capabilities = context
        .meta
        .contains_key(EXECUTION_CONTEXT_NAMESPACE)
        .then(|| EXECUTION_CONTEXT_NAMESPACE.to_string())
        .into_iter()
        .collect();
    let input_responses = match &request.input_responses {
        Some(responses) => crate::mrtr::responses_from_mrtr(responses).map_err(|error| {
            ErrorData::invalid_params(format!("cannot use inputResponses: {error}"), None)
        })?,
        None => Default::default(),
    };
    let request_state = crate::mrtr::request_state_from_mrtr(request.request_state.as_deref())
        .map_err(|error| {
            ErrorData::invalid_params(format!("cannot use requestState: {error}"), None)
        })?;
    Ok(ToolInvocationContext {
        run_context: None,
        call_id: format!("{:?}", context.id),
        invoking_session_id: None,
        invoking_session: None,
        capabilities,
        checkpoint: None,
        checkpoint_store: None,
        partial_result_store: None,
        progress: Default::default(),
        new_message: Default::default(),
        input_responses,
        request_state,
    })
}

pub(super) fn call_tool_result_from_value(value: Value) -> CallToolResult {
    if let Ok(result) = serde_json::from_value::<CallToolResult>(value.clone()) {
        return result;
    }
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    CallToolResult::success(vec![ContentBlock::text(text)])
}
