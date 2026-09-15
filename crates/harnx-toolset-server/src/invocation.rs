//! Admission, reply delivery and server-owned handler cleanup.
use super::*;

pub(super) async fn invoke_uncached_tool(
    context: &ToolRequestContext,
    request: &ToolRequest,
    parent_cx: OtelContext,
) -> Result<Value, ToolInvokeError> {
    let recovery = recovery::InvocationRecovery::load(context, request).await?;
    if let Some(reply) = recovery.completed_reply(context).await? {
        return recovery::reply_result(reply);
    }
    recovery.check_policy(context).await?;
    let execution = execution::InvocationExecution::claim(
        &context.execution_store,
        request,
        &context.server_identity,
    )
    .await
    .map_err(reply_fence::invoke_error)?;
    invoke_claimed(context, request, parent_cx, (recovery, execution)).await
}

async fn invoke_claimed(
    context: &ToolRequestContext,
    request: &ToolRequest,
    parent_cx: OtelContext,
    (recovery, execution): (recovery::InvocationRecovery, execution::InvocationExecution),
) -> Result<Value, ToolInvokeError> {
    let cancel = CancellationToken::new();
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    context.in_flight.lock().await.insert(
        request.call_id.clone(),
        execution::ActiveCall {
            producer: execution.producer.clone(),
            cancel: cancel.clone(),
        },
    );
    let metric_tool = metric_tool_name(context.toolset.as_ref(), &request.tool);
    let start = Instant::now();
    let guarantee = cancellation_guarantee(context, &request.tool);
    let invocation = invocation(request, &execution, &cancel);
    let toolset = context.toolset.clone();
    let in_flight = context.in_flight.clone();
    let producer = execution.producer.clone();
    let call_id = request.call_id.clone();
    let span = tool_exec_span(&request.tool, parent_cx);
    context.cleanup.spawn(async move {
        let invocation = Box::pin(
            recovery
                .invoke(toolset.as_ref(), invocation, producer.clone())
                .instrument(span),
        );
        execution
            .invoke(cancel, guarantee, invocation, reply_tx)
            .await;
        let mut active = in_flight.lock().await;
        if active
            .get(&call_id)
            .is_some_and(|call| call.producer == producer)
        {
            active.remove(&call_id);
        }
    });
    let result = reply_rx.await.unwrap_or_else(|error| {
        Err(ToolInvokeError::Fatal(format!(
            "tool owner task lost: {error}"
        )))
    });
    let elapsed = start.elapsed();
    let is_ok = result.is_ok();
    harnx_metrics::record_tool_call(metric_tool, is_ok, elapsed);
    result
}

fn invocation(
    request: &ToolRequest,
    execution: &execution::InvocationExecution,
    cancel: &CancellationToken,
) -> ToolInvocation {
    let mut args = request.args.clone();
    let invocation_context = ToolInvocationContext {
        operation: Some(execution.reference.clone()),
        execution: Some(execution.producer.clone()),
        call_id: request.call_id.clone(),
        invoking_session_id: request.parent_session_id.clone(),
        capabilities: request.capabilities.clone(),
    };
    add_parent_context_args(
        &request.tool,
        request.parent_session_id.clone(),
        request.tool_call_id.clone(),
        &mut args,
    );
    ToolInvocation {
        tool: request.tool.clone(),
        args,
        context: invocation_context,
        cancel: cancel.clone(),
    }
}

pub(super) fn tool_exec_span(tool_name: &str, parent_cx: OtelContext) -> tracing::Span {
    let span = tracing::info_span!(
        "tool_exec",
        otel.kind = "server",
        harnx.tool.name = tool_name,
    );
    harnx_telemetry::set_span_parent(&span, parent_cx);
    span
}

pub(super) fn metric_tool_name<'a>(toolset: &dyn Toolset, requested: &'a str) -> &'a str {
    if toolset.tools().iter().any(|tool| tool.name == requested) {
        requested
    } else {
        "unknown"
    }
}

fn cancellation_guarantee(
    context: &ToolRequestContext,
    tool: &str,
) -> harnx_toolset::CancellationGuarantee {
    context
        .toolset
        .tools()
        .iter()
        .find(|spec| spec.name == tool)
        .map(|spec| spec.cancellation_guarantee)
        .unwrap_or_default()
}

pub(super) fn add_parent_context_args(
    tool: &str,
    parent_session_id: Option<String>,
    tool_call_id: Option<String>,
    args: &mut Value,
) {
    let Some(args) = args.as_object_mut() else {
        return;
    };
    // These are transport-owned arguments. Always discard model-supplied
    // values before optionally replacing them with context from ToolRequest.
    args.remove("__harnx_parent_session_id");
    args.remove("__harnx_tool_call_id");
    if let Some(parent_session_id) = parent_session_id.filter(|_| accepts_parent_session_id(tool)) {
        args.insert(
            "__harnx_parent_session_id".to_string(),
            Value::String(parent_session_id),
        );
        if let Some(tool_call_id) = tool_call_id {
            args.insert(
                "__harnx_tool_call_id".to_string(),
                Value::String(tool_call_id),
            );
        }
    }
}

pub(super) fn accepts_parent_session_id(tool: &str) -> bool {
    // Sub-agent toolsets reserve these raw names for calls that start a child turn.
    matches!(
        tool,
        SUBAGENT_SESSION_PROMPT_TOOL | SUBAGENT_SESSION_NEW_TOOL
    )
}
