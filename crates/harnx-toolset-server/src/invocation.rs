//! Admission, reply delivery and server-owned handler cleanup.
use super::*;
use invocation_journal::JournalCheckpointStore;

pub(super) async fn invoke_uncached_tool(
    context: &ToolRequestContext,
    request: &ToolRequest,
    parent_cx: OtelContext,
) -> Result<Value, ToolInvokeError> {
    let recovery = recovery::InvocationRecovery::load(context, request).await?;
    if let Some(reply) = recovery.completed_reply().await? {
        return recovery::reply_result(reply);
    }
    recovery.check_policy(context.toolset.as_ref())?;
    let execution =
        execution::InvocationExecution::claim(request).map_err(recovery::invoke_error)?;
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
    let active = execution.active_call(cancel.clone());
    context
        .in_flight
        .lock()
        .await
        .insert(request.call_id.clone(), active.clone());
    let metric_tool = metric_tool_name(context.toolset.as_ref(), &request.tool);
    let start = Instant::now();
    let guarantee = cancellation_guarantee(context, &request.tool);
    let invocation = invocation(request, &recovery, &execution, &cancel);
    let toolset = context.toolset.clone();
    let in_flight = context.in_flight.clone();
    let call_id = request.call_id.clone();
    let span = tool_exec_span(&request.tool, parent_cx);
    context.cleanup.spawn(async move {
        let invocation = Box::pin(
            recovery
                .invoke(toolset.as_ref(), invocation)
                .instrument(span),
        );
        execution
            .invoke(cancel, guarantee, invocation, reply_tx)
            .await;
        let mut in_flight = in_flight.lock().await;
        if in_flight
            .get(&call_id)
            .is_some_and(|current| current.is_same_call(&active))
        {
            in_flight.remove(&call_id);
        }
    });
    let result = match reply_rx.await {
        Ok(outcome) => record_outcome(context, request, outcome).await,
        // A lost owner task reported no outcome, so there is nothing to record:
        // whether the tool ran at all is exactly what this process cannot say.
        Err(error) => Err(ToolInvokeError::Fatal(format!(
            "tool owner task lost: {error}"
        ))),
    };
    let elapsed = start.elapsed();
    let is_ok = result.is_ok();
    harnx_metrics::record_tool_call(metric_tool, is_ok, elapsed);
    result
}

/// The outcome the owner task reports is the one the caller is about to be
/// given, so record it here, before returning it, and return whatever the
/// journal says won. This is the only place a reply is written: a handler
/// drained after its call was answered as interrupted never reaches it, and
/// neither does a success whose cancellation landed a moment too late.
async fn record_outcome(
    context: &ToolRequestContext,
    request: &ToolRequest,
    outcome: Result<Value, ToolInvokeError>,
) -> Result<Value, ToolInvokeError> {
    let reply = ToolReply {
        call_id: request.call_id.clone(),
        result: outcome.map_err(map_invoke_error),
    };
    match context.journal.complete(request, reply).await {
        Ok(durable) => recovery::reply_result(durable),
        Err(error) => Err(recovery::invoke_error(error)),
    }
}

fn invocation(
    request: &ToolRequest,
    recovery: &recovery::InvocationRecovery,
    execution: &execution::InvocationExecution,
    cancel: &CancellationToken,
) -> ToolInvocation {
    let mut args = request.args.clone();
    let invocation_context = ToolInvocationContext {
        call_id: request.call_id.clone(),
        invoking_session_id: request.parent_session_id.clone(),
        capabilities: request.capabilities.clone(),
        checkpoint: recovery.checkpoint(),
        checkpoint_store: Some(Arc::new(JournalCheckpointStore {
            journal: recovery.journal(),
            session: execution.session_id.clone(),
            call_id: request.call_id.clone(),
        })),
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

/// Build the invocation `Toolset::cancel` runs against for a call this
/// process is not running. The journal row is the only source of truth left,
/// so the token starts already cancelled (there is no live handler to signal)
/// and the checkpoint comes from that row instead of a live
/// `InvocationRecovery`.
pub(super) fn orphan_invocation(
    context: &ToolRequestContext,
    request: &ToolRequest,
    checkpoint: Option<Value>,
) -> ToolInvocation {
    let mut args = request.args.clone();
    let session = request
        .parent_session_id
        .clone()
        .unwrap_or_else(|| request.call_id.clone());
    let invocation_context = ToolInvocationContext {
        call_id: request.call_id.clone(),
        invoking_session_id: request.parent_session_id.clone(),
        capabilities: request.capabilities.clone(),
        checkpoint,
        checkpoint_store: Some(Arc::new(JournalCheckpointStore {
            journal: context.journal.clone(),
            session,
            call_id: request.call_id.clone(),
        })),
    };
    add_parent_context_args(
        &request.tool,
        request.parent_session_id.clone(),
        request.tool_call_id.clone(),
        &mut args,
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    ToolInvocation {
        tool: request.tool.clone(),
        args,
        context: invocation_context,
        cancel,
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
