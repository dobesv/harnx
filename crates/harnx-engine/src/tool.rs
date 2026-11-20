//! Tool-call loop — `eval_tool_calls` orchestrates a batch of tool
//! dispatches with middleware, durable interactions, UI emission,
//! and per-call abort handling. The loop is provider-agnostic: it
//! iterates `Vec<Arc<dyn ToolProvider>>` (harnx-core trait). All
//! harnx-specific concerns — UI rendering, middleware execution,
//! inquire prompts — are injected via callbacks on `ToolEvalContext`,
//! constructed on the harnx side by `build_tool_eval_context`.

use anyhow::{anyhow, Result};

use harnx_core::abort::AbortSignal;
use harnx_core::partial_result::{output_with_partial_result, partial_result_of};
use harnx_core::tool::{
    SwitchAgentData, ToolCall, ToolCallOrigin, ToolError, ToolProvider, ToolProviderOutput,
    ToolResult, ToolUpdatePatch,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::Instrument;

mod interaction;
pub use interaction::{
    eval_tool_round, eval_tool_round_with_authorization, PendingToolInteraction,
    ToolCallCheckpoint, ToolCallCompletedFn, ToolInputRequiredError, ToolInteractionFn,
    ToolRoundFailure, ToolRoundOutcome, ToolRoundSuspension,
};
mod middleware;
use middleware::{materialize_dispatch_result, run_middleware};
pub use middleware::{
    MaterializedToolResult, PreparedToolCall, ToolCallNext, ToolMiddlewareContext,
    ToolMiddlewareFn, ToolMiddlewareFuture, ToolMiddlewareOrigin, ToolMiddlewareOutcome,
};

/// Callback invoked with a `&ToolCall` and the parsed arguments JSON.
/// Used for both "tool is about to dispatch" and "tool returned a
/// result" UI emission hooks on `ToolEvalContext`.
pub type ToolCallEmitFn = dyn Fn(&ToolCall, &Value) + Send + Sync;

/// Callback invoked when a tool emits a progress update during execution.
/// Receives the tool-call ID and the update patch. The runtime supplies the ID;
/// tools call `progress.update(patch)` without knowing their call identity.
pub type ToolUpdateEmitFn = dyn Fn(&str, &ToolUpdatePatch) + Send + Sync;

/// Frontend bridge invoked after a durable interaction is ready to show.
/// The callback gets full `ToolCall` identity plus parsed args so
/// callers can approve, deny, or defer execution.
pub type ConfirmToolUseFn =
    dyn Fn(&ToolCall, &Value, Option<&str>) -> ToolUseConfirmation + Send + Sync;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolUseConfirmation {
    Approve,
    Deny { reason: Option<String> },
    Defer,
}

/// Result of preparing a handoff through the optional callback.
///
/// The preparation callback is invoked by the engine after target resolution
/// and before building the handoff result. It returns the resolved agent and
/// session ID. Runtime-specific context stays in callback closure state.
#[derive(Debug, Clone)]
pub struct PreparedHandoff {
    /// The routable agent reference (cluster-qualified where required).
    pub agent: String,
    /// The resolved session ID for the child session.
    pub session_id: String,
}

/// Callback invoked before building a handoff result, after target resolution.
///
/// The callback receives:
/// - `target_agent`: The resolved routable agent reference
/// - `requested_session_id`: The caller-supplied session ID, if any
/// - `tool_call_id`: The tool call ID for this handoff
///
/// Returns `Ok(PreparedHandoff)` on success, or `Err` if preparation/reservation
/// fails. Runtime-specific context can be kept in callback closure state.
/// Per the contract, reservation failures remain fatal and do not become
/// recoverable tool errors.
pub type PrepareHandoffFn = dyn Fn(
        String,         // target_agent
        Option<String>, // requested_session_id
        String,         // tool_call_id
    ) -> Pin<Box<dyn Future<Output = Result<PreparedHandoff>> + Send>>
    + Send
    + Sync;

#[derive(Clone)]
pub struct ToolEvalRenderContext {
    pub decl_map: Arc<HashMap<String, harnx_core::tool::ToolDeclaration>>,
}

#[derive(Clone, Copy)]
pub enum WorkBoundary {
    Start,
    Accept,
}

/// Runtime implements this with the creation-time execution gate, never a GET.
pub type WorkBoundaryFn = dyn Fn(WorkBoundary, ToolCall) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>
    + Send
    + Sync;

#[derive(Clone)]
pub struct ToolEvalContext {
    /// Wrap authorized model, inner, and operator calls.
    pub middleware_fn: Option<Arc<ToolMiddlewareFn>>,
    /// Effective per-call completion before terminal publication (including
    /// admission errors). Runtime renders and durably checkpoints here.
    pub on_tool_call_completed: Option<Arc<ToolCallCompletedFn>>,
    /// Round-level suspension, called only after all sibling completions settle.
    pub on_interaction: Option<Arc<ToolInteractionFn>>,
    pub work_boundary: Option<Arc<WorkBoundaryFn>>,
    /// Worker instance that scopes NATS tool subjects for this evaluation.
    pub instance_id: harnx_core::instance::ServerScope,
    /// Sequence of the session log's `ToolCalls` entry that made the calls
    /// being evaluated, handed to every provider dispatch. `None` when no
    /// transcript round made them. Replay and wind-up find a call's journal
    /// row only by this round, so a path that dispatches a transcript round's
    /// calls must set it; left unset, the rows record round zero and recovery
    /// never finds them.
    pub tool_round: Option<u64>,
    pub render: Option<ToolEvalRenderContext>,
    /// Ordered tool providers to search when dispatching a call.
    pub providers: Vec<Arc<dyn ToolProvider>>,
    /// Allow-list of synthetic tool names that do not come from a real
    /// provider but are handled directly in `dispatch_tool_call`
    /// (currently `_session_handoff`).
    pub allowed_tool_names: HashSet<String>,
    /// Package of the currently active agent (e.g. `Some("pantheon")` for
    /// `pantheon/daedalus`, `None` for a top-level agent). Used to resolve
    /// bare `_session_handoff` targets relative to the current package so a
    /// same-package handoff (`atlas_session_handoff`) lands on
    /// `pantheon/atlas` rather than top-level `atlas` (#709).
    pub current_agent_package: Option<String>,
    /// Exact decode table for package-aware handoff display names.
    /// Maps tool display name without `_session_handoff` suffix to
    /// qualified-or-bare target agent name.
    pub handoff_targets: HashMap<String, String>,
    /// Called when a tool is about to be dispatched. Receives the tool
    /// call and the parsed arguments JSON. Harnx's default emits an
    /// `AgentEvent::Tool(Started { .. })` via the unified AgentEvent
    /// sink, falling back to stdout if no sink is installed.
    pub emit_tool_call_fn: Arc<ToolCallEmitFn>,
    /// Called when a tool call returns a result. Receives the tool
    /// call and the raw result JSON. Harnx's default emits an
    /// `AgentEvent::Tool(Completed { .. })` via the unified AgentEvent
    /// sink; when no sink is installed it extracts user-display text
    /// (or YAML-pretty-prints the JSON), truncates to terminal
    /// dimensions, dims the text, and writes to stdout.
    pub emit_tool_result_fn: Arc<ToolCallEmitFn>,
    /// Called when middleware blocks a tool call.
    /// Receives the tool call and the blocked result JSON (contains
    /// `"error"` key). Harnx's default emits
    /// `AgentEvent::Tool(ToolEvent::Blocked { .. })`.
    pub emit_tool_blocked_fn: Arc<ToolCallEmitFn>,
    /// Called when a tool emits a progress update during execution.
    /// Receives the tool-call ID and the update patch. Harnx's default
    /// emits `AgentEvent::Tool(ToolEvent::Update { .. })` via the unified
    /// AgentEvent sink. Runtime coalesces rapid updates with a 250ms budget.
    pub emit_tool_update_fn: Arc<ToolUpdateEmitFn>,
    /// Frontend confirmation bridge for durable middleware interactions.
    /// Runtime persists the request before calling this bridge and durably
    /// accepts its answer. `Defer` leaves the request pending.
    pub confirm_tool_use_fn: Arc<ConfirmToolUseFn>,
    /// Optional callback invoked after target resolution and before building
    /// the handoff result. The runtime implements this to reserve a child
    /// session ID and carry provenance through to dispatch.
    ///
    /// When present:
    /// - The handoff result includes `handoff: {agent, session_id}`
    /// - The text includes "target session: <id>"
    /// - `SwitchAgentData.session_id` reflects the resolved id
    ///
    /// When absent (engine-only tests):
    /// - Legacy behavior: session_id = caller value or null
    /// - `handoff` marker only when an id exists
    pub prepare_handoff_fn: Option<Arc<PrepareHandoffFn>>,
}

/// Required replacements when forking a context for script-originated calls.
/// All emitters are supplied explicitly so inner calls don't inherit UI rows.
pub struct InnerToolEvalOptions {
    /// Base providers, excluding the script provider to prevent recursive dispatch.
    /// The caller still enforces the inner tool-name allowlist before evaluation.
    pub providers: Vec<Arc<dyn ToolProvider>>,
    /// Parent invocation's `ToolCallOrigin::tool_round`, supplied at call time.
    pub tool_round: Option<u64>,
    /// Collect the admitted call and its parsed arguments.
    pub emit_tool_call_fn: Arc<ToolCallEmitFn>,
    /// Collect the completed call and its output, including recoverable errors.
    pub emit_tool_result_fn: Arc<ToolCallEmitFn>,
    /// Collect a blocked call without emitting a child UI row.
    pub emit_tool_blocked_fn: Arc<ToolCallEmitFn>,
    /// Route per-call progress into the parent call's aggregator.
    pub emit_tool_update_fn: Arc<ToolUpdateEmitFn>,
}

/// Typed outcome of one evaluated call, derived from the engine's own
/// control flow rather than from output JSON keys, which a successful
/// provider output could contain incidentally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InnerToolCallStatus {
    /// Dispatched and completed without a provider failure.
    Ok,
    /// Dispatched but failed recoverably (provider error or unparsable args).
    ToolError,
    /// Middleware blocked the call; never dispatched.
    Blocked,
}

/// One evaluated inner call: the lossless result plus its typed status.
#[derive(Debug)]
pub struct EvaluatedInnerCall {
    pub result: ToolResult,
    pub status: InnerToolCallStatus,
}

impl ToolEvalContext {
    /// Fork admission state without inheriting operator consent or UI emitters.
    /// Hooks and work-boundary checks are shared; metadata is cloned. The outer
    /// context is unchanged. The supplied round replaces even a stale outer round.
    /// The handoff preparation callback is dropped: inner calls must not make
    /// durable child-session reservations. The engine still routes handoff
    /// tool names, so the fork's caller remains responsible for rejecting
    /// `*_session_handoff` before admission.
    ///
    /// Cancellation isn't stored in `ToolEvalContext`: pass the parent's existing
    /// `AbortSignal` to [`eval_inner_tool_call`] when evaluating the fork.
    pub fn fork_for_inner_calls(&self, options: InnerToolEvalOptions) -> Self {
        Self {
            middleware_fn: self.middleware_fn.clone(),
            // Inner calls don't own transcript outer completions or suspension.
            on_tool_call_completed: None,
            on_interaction: None,
            work_boundary: self.work_boundary.clone(),
            instance_id: self.instance_id.clone(),
            tool_round: options.tool_round,
            render: self.render.clone(),
            providers: options.providers,
            allowed_tool_names: self.allowed_tool_names.clone(),
            current_agent_package: self.current_agent_package.clone(),
            handoff_targets: self.handoff_targets.clone(),
            emit_tool_call_fn: options.emit_tool_call_fn,
            emit_tool_result_fn: options.emit_tool_result_fn,
            emit_tool_blocked_fn: options.emit_tool_blocked_fn,
            emit_tool_update_fn: options.emit_tool_update_fn,
            confirm_tool_use_fn: Arc::new(|_, _, _| ToolUseConfirmation::Defer),
            prepare_handoff_fn: None,
        }
    }

    /// What a dispatch of the call `tool_call_id` tells its provider about the
    /// transcript call it answers.
    fn origin<'a>(&self, tool_call_id: &'a str) -> ToolCallOrigin<'a> {
        ToolCallOrigin {
            tool_call_id: Some(tool_call_id),
            tool_round: self.tool_round,
        }
    }
}

struct ApprovedToolCall {
    call: ToolCall,
    json_data: Value,
}

impl ApprovedToolCall {
    fn effective_call(&self) -> ToolCall {
        let mut call = self.call.clone();
        call.arguments = self.json_data.clone();
        call
    }
}

pub async fn eval_tool_calls(
    ctx: &ToolEvalContext,
    calls: Vec<ToolCall>,
    abort_signal: &AbortSignal,
) -> Result<Vec<ToolResult>> {
    eval_tool_calls_with_authorization(ctx, calls, abort_signal, None).await
}

/// Assign stable, non-empty identity to calls that don't already have it.
///
/// Runtime invokes this before persistence. Evaluation invokes it again as a
/// safety net for direct engine callers; existing IDs remain unchanged.
pub fn ensure_tool_call_ids(calls: &mut [ToolCall]) {
    for call in calls {
        if call.id.as_deref().is_none_or(|id| id.trim().is_empty()) {
            call.id = Some(uuid::Uuid::new_v4().to_string());
        }
    }
}

/// Recheck the recovering worker's lease immediately before each dispatch.
pub async fn eval_tool_calls_with_authorization(
    ctx: &ToolEvalContext,
    calls: Vec<ToolCall>,
    abort_signal: &AbortSignal,
    authorization: Option<&dyn harnx_core::tool::ReplayAuthorization>,
) -> Result<Vec<ToolResult>> {
    let evaluated = eval_tool_calls_inner(
        ToolEvaluation {
            ctx,
            abort_signal,
            authorization,
            lossless: false,
            origin: ToolMiddlewareOrigin::Model,
        },
        calls,
    )
    .await?;
    // Batch callers keep the plain result list; the typed status is only
    // consumed by the inner-call entry.
    Ok(evaluated.into_iter().map(|call| call.result).collect())
}

/// Evaluate one explicit operator call through middleware and providers.
/// Consent applies only to this call ID; it never changes shared configuration
/// or the confirmation callback used by nested agent evaluations.
pub async fn eval_operator_tool_call(
    ctx: ToolEvalContext,
    mut call: ToolCall,
    abort_signal: &AbortSignal,
) -> Result<ToolResult> {
    anyhow::ensure!(
        ctx.allowed_tool_names.contains(&call.name),
        "Tool '{}' is not allowed in this context",
        call.name
    );
    ensure_tool_call_ids(std::slice::from_mut(&mut call));
    let mut results = eval_tool_calls_inner(
        ToolEvaluation {
            ctx: &ctx,
            abort_signal,
            authorization: None,
            lossless: true,
            origin: ToolMiddlewareOrigin::Operator,
        },
        vec![call],
    )
    .await?;
    anyhow::ensure!(
        results.len() == 1,
        "operator tool evaluation returned no result"
    );
    Ok(results.remove(0).result)
}

/// Evaluate one script-originated call through the normal admission pipeline.
/// Unlike [`eval_operator_tool_call`], this leaves the context's confirmation
/// callback unchanged. A durable confirmation returns an interaction-required result;
/// fatal errors propagate, while blocks and recoverable failures return tool results.
/// Null outputs and inline media are preserved.
///
/// The caller must enforce its inner-tool allowlist before calling this function.
/// Lifecycle and progress events use the context's callbacks, so callers can
/// collect a trace without emitting child UI rows. This does not persist a round;
/// providers receive `ctx.tool_round` and the call's ID (assigned here if missing).
/// The context is borrowed immutably and can be shared by concurrent callers.
///
/// The returned [`InnerToolCallStatus`] classifies the call from the engine's
/// own control flow; don't infer it from the output's JSON keys.
pub async fn eval_inner_tool_call(
    ctx: &ToolEvalContext,
    call: ToolCall,
    abort_signal: &AbortSignal,
) -> Result<EvaluatedInnerCall> {
    let mut results = Box::pin(eval_tool_calls_inner(
        ToolEvaluation {
            ctx,
            abort_signal,
            authorization: None,
            lossless: true,
            origin: ToolMiddlewareOrigin::Inner,
        },
        vec![call],
    ))
    .await?;
    anyhow::ensure!(
        results.len() == 1,
        "inner tool evaluation returned no result"
    );
    Ok(results.remove(0))
}

#[derive(Clone, Copy)]
struct ToolEvaluation<'a> {
    ctx: &'a ToolEvalContext,
    abort_signal: &'a AbortSignal,
    authorization: Option<&'a dyn harnx_core::tool::ReplayAuthorization>,
    lossless: bool,
    origin: ToolMiddlewareOrigin,
}

enum ToolAdmission {
    Approved(Box<ApprovedToolCall>),
    /// An admission-time result (blocked or unparsable args) with its typed status.
    Immediate(Box<ToolResult>, InnerToolCallStatus),
}

async fn eval_tool_calls_inner(
    evaluation: ToolEvaluation<'_>,
    calls: Vec<ToolCall>,
) -> Result<Vec<EvaluatedInnerCall>> {
    interaction::evaluate_round(evaluation, calls, vec![])
        .await?
        .into_legacy(evaluation.lossless)
}

async fn prepare_tool_call(
    ctx: &ToolEvalContext,
    call: ToolCall,
    abort_signal: &AbortSignal,
) -> Result<ToolAdmission> {
    work_boundary(ctx, &call, abort_signal, WorkBoundary::Accept).await?;
    let json_data = match parse_call_arguments(&call) {
        Ok(json_data) => json_data,
        Err(ToolError::Recoverable(err)) => {
            let result = json!({"is_error": true, "error": format!("{err:#}")});
            return Ok(ToolAdmission::Immediate(
                Box::new(ToolResult::new(call, result)),
                // Unparsable arguments are a recoverable tool failure.
                InnerToolCallStatus::ToolError,
            ));
        }
        Err(ToolError::Fatal(err)) => return Err(err),
    };
    let approved = ApprovedToolCall { call, json_data };
    (ctx.emit_tool_call_fn)(&approved.call, &approved.json_data);
    Ok(ToolAdmission::Approved(Box::new(approved)))
}

async fn dispatch_authorized_call(
    call: &ApprovedToolCall,
    ctx: &ToolEvalContext,
    abort_signal: &AbortSignal,
    authorization: Option<&dyn harnx_core::tool::ReplayAuthorization>,
) -> Result<ToolProviderOutput, ToolError> {
    if abort_signal.aborted() {
        return Err(ToolError::Fatal(anyhow!(
            "interrupted during tool execution"
        )));
    }
    if let Some(authorization) = authorization {
        authorization.revalidate().await.map_err(ToolError::Fatal)?;
    }
    let admitted_call = call.effective_call();
    work_boundary(ctx, &admitted_call, abort_signal, WorkBoundary::Start)
        .await
        .map_err(ToolError::Fatal)?;
    dispatch_tool_call(call.call.clone(), call.json_data.clone(), ctx, abort_signal).await
}

async fn materialize_failed_tool_call(
    ctx: &ToolEvalContext,
    call: &ApprovedToolCall,
    error: anyhow::Error,
    abort: &AbortSignal,
) -> Result<ToolResult> {
    let error_display = format!("{error:#}");
    work_boundary(ctx, &call.call, abort, WorkBoundary::Accept).await?;
    let value = output_with_partial_result(
        json!({"is_error": true, "error": error_display}),
        partial_result_of(&error),
    );
    Ok(ToolResult::new(call.call.clone(), value))
}

async fn work_boundary(
    ctx: &ToolEvalContext,
    call: &ToolCall,
    abort: &AbortSignal,
    boundary: WorkBoundary,
) -> Result<()> {
    anyhow::ensure!(!abort.aborted(), "interrupted during tool execution");
    if let Some(commit) = &ctx.work_boundary {
        commit(boundary, call.clone()).await?;
    }
    anyhow::ensure!(!abort.aborted(), "interrupted during tool execution");
    Ok(())
}

async fn materialize_successful_tool_call(
    evaluation: &ToolEvaluation<'_>,
    approved: &ApprovedToolCall,
    provider_output: ToolProviderOutput,
) -> Result<(ToolResult, bool)> {
    let ToolEvaluation {
        ctx,
        abort_signal: abort,
        lossless,
        ..
    } = *evaluation;
    let (mut value, execution_context) = provider_output.into_parts();
    work_boundary(ctx, &approved.call, abort, WorkBoundary::Accept).await?;
    let lossless_media = lossless;
    // Remove private surfaces first: image projection errors may wrap the
    // remaining output for diagnostics, which must not preserve user-only UI.
    let surfaces = if lossless_media {
        Vec::new()
    } else {
        crate::a2ui::prepare_tool_result_surfaces(&mut value)
    };
    let images = if lossless_media {
        vec![]
    } else {
        crate::media::prepare_tool_result_images(&mut value)
    };
    if !images.is_empty() {
        crate::media::redact_image_data(&mut value);
    }
    let was_null = value.is_null();
    if was_null && !lossless {
        value = json!("DONE");
    }
    Ok((
        completed_tool_result(
            approved.call.clone(),
            value,
            images,
            surfaces,
            execution_context,
        ),
        was_null,
    ))
}

/// Publish only the effective outer result. Handoff selection/activation is a
/// runtime round-level operation, never a side effect of materialization.
fn publish_tool_call(
    evaluation: &ToolEvaluation<'_>,
    approved: &ApprovedToolCall,
    mut completed: MaterializedToolResult,
) -> EvaluatedInnerCall {
    normalize_outer_result(evaluation, &approved.call, &mut completed);
    let was_null = completed.was_null();
    let published = if was_null {
        &Value::Null
    } else {
        &completed.result.output
    };
    (evaluation.ctx.emit_tool_result_fn)(&approved.call, published);
    if was_null && !evaluation.lossless {
        completed.result.output = json!("DONE");
    }
    EvaluatedInnerCall {
        result: completed.result,
        status: completed.status,
    }
}

/// Recover a journaled response without executing the provider again.
/// Middleware-owned calls use durable checkpoints instead.
pub async fn replay_tool_call(
    ctx: &ToolEvalContext,
    replay: harnx_core::tool::ToolReplay<'_>,
    abort: &AbortSignal,
) -> anyhow::Result<Option<ToolResult>> {
    let call = replay.call;
    for provider in &ctx.providers {
        let output = match provider.replay_tool_call(replay, abort).await {
            Ok(Some(output)) => output,
            Ok(None) => continue,
            Err(ToolError::Fatal(error)) => return Err(error),
            Err(ToolError::Recoverable(error)) => {
                ToolProviderOutput::new(json!({"is_error": true, "error": error.to_string()}))
            }
        };
        if output.input_required.is_some() {
            // The journaled reply was a challenge, not a result. Its round is
            // interaction-owned and reconciled by the durable path; replaying
            // a challenge as a result would silently answer nothing.
            return Ok(Some(ToolResult::new(
                call.clone(),
                json!({
                    "is_error": true,
                    "error": "interrupted tool call was waiting for user input; its result is not available"
                }),
            )));
        }
        work_boundary(ctx, call, abort, WorkBoundary::Accept).await?;
        let (mut value, observation) = output.into_parts();
        let surfaces = crate::a2ui::prepare_tool_result_surfaces(&mut value);
        let images = crate::media::prepare_tool_result_images(&mut value);
        if !images.is_empty() {
            crate::media::redact_image_data(&mut value);
        }
        (ctx.emit_tool_result_fn)(call, &value);
        if value.is_null() {
            value = json!("DONE");
        }
        return Ok(Some(completed_tool_result(
            call.clone(),
            value,
            images,
            surfaces,
            observation,
        )));
    }
    Ok(None)
}

fn parse_call_arguments(call: &ToolCall) -> Result<Value, ToolError> {
    if call.arguments.is_null() {
        return Ok(Value::Null);
    }
    if call.arguments.is_object() {
        return Ok(call.arguments.clone());
    }
    if let Some(arguments) = call.arguments.as_str() {
        return serde_json::from_str(arguments).map_err(|_| {
            ToolError::Recoverable(anyhow!(
                "The call '{}' has invalid arguments: {arguments}",
                call.name
            ))
        });
    }
    Err(ToolError::Recoverable(anyhow!(
        "The call '{}' has invalid arguments: {}",
        call.name,
        call.arguments
    )))
}

fn completed_tool_result(
    call: ToolCall,
    output: Value,
    content: Vec<harnx_core::message::MessageContentPart>,
    user_surfaces: Vec<Value>,
    execution_context: Option<harnx_core::execution_context::ExecutionContextObservation>,
) -> ToolResult {
    let tool_call_id = call.id.clone();
    let mut result = ToolResult::new(call, output);
    result.content = content;
    result.user_surfaces = user_surfaces;
    result.switch_agent = detect_switch_agent(&result.output);
    if let Some(switch_agent) = &mut result.switch_agent {
        if switch_agent.tool_call_id.is_none() {
            switch_agent.tool_call_id = tool_call_id;
        }
    }
    result.execution_context = execution_context;
    result
}

fn detect_switch_agent(output: &Value) -> Option<SwitchAgentData> {
    let obj = output.as_object()?;
    if obj.get("action").and_then(|v| v.as_str()) != Some("switch_agent") {
        return None;
    }
    let agent = obj.get("agent").and_then(|v| v.as_str())?;
    let prompt = obj.get("prompt").and_then(|v| v.as_str())?;
    Some(SwitchAgentData {
        agent: agent.to_string(),
        prompt: prompt.to_string(),
        session_id: obj
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|session_id| !session_id.trim().is_empty())
            .map(ToString::to_string),
        tool_call_id: obj
            .get("tool_call_id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.trim().is_empty())
            .map(ToString::to_string),
    })
}

async fn call_tool_with_tracing(
    provider: &dyn ToolProvider,
    tool_name: &str,
    json_data: Value,
    origin: ToolCallOrigin<'_>,
    abort_signal: &AbortSignal,
    progress: Arc<dyn harnx_core::tool::ToolProgress>,
) -> Result<ToolProviderOutput, ToolError> {
    let span = tracing::info_span!(
        "tool_call",
        otel.kind = "client",
        harnx.tool.name = tool_name,
        harnx.tool.arguments_bytes = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
    );
    if !span.is_disabled() {
        let arguments_bytes = serde_json::to_vec(&json_data)
            .map(|arguments| i64::try_from(arguments.len()).unwrap_or(i64::MAX))
            .unwrap_or_default();
        span.record("harnx.tool.arguments_bytes", arguments_bytes);
    }
    let result = provider
        .call_tool_with_progress(tool_name, json_data, origin, abort_signal, progress)
        .instrument(span.clone())
        .await;
    if result.is_err() {
        span.record("otel.status_code", "ERROR");
    }
    result
}

fn handoff_agent(tool_name: &str, ctx: &ToolEvalContext) -> String {
    // Strip exactly one `_session_handoff` suffix (not all repeats, which
    // `trim_end_matches` would do) so an agent named `*_session_handoff`
    // resolves correctly.
    let bare_target = tool_name
        .strip_suffix("_session_handoff")
        .unwrap_or(tool_name);
    // Resolve package-aware display names through the exact lookup table
    // first. Map values are already the canonical agent name ("pkg/stem"
    // for package agents, bare "stem" for top-level agents), so they are
    // used verbatim — re-resolving a bare top-level value against the
    // current package would wrongly qualify it (e.g. `global` →
    // `pantheon/global`). Only the legacy fallback path (no map entry,
    // e.g. test/no-context contexts) applies package-relative resolution.
    match ctx.handoff_targets.get(bare_target) {
        Some(mapped) => mapped.clone(),
        None => harnx_core::package_namespace::resolve_package_relative_name(
            bare_target,
            ctx.current_agent_package.as_deref(),
        ),
    }
}

async fn dispatch_session_handoff(
    call: ToolCall,
    json_data: Value,
    ctx: &ToolEvalContext,
) -> Result<ToolProviderOutput, ToolError> {
    if !ctx.allowed_tool_names.contains(&call.name) {
        return Err(ToolError::Recoverable(anyhow!(
            "No tool provider configured for '{}'",
            call.name
        )));
    }
    let agent = handoff_agent(&call.name, ctx);
    let prompt = json_data["prompt"].as_str().ok_or_else(|| {
        ToolError::Recoverable(anyhow!("Missing 'prompt' argument for session handoff"))
    })?;
    let caller_session_id = json_data["session_id"]
        .as_str()
        .filter(|session_id| !session_id.trim().is_empty())
        .map(ToString::to_string);

    // Tool call ID is guaranteed to be assigned by ensure_tool_call_ids above.
    let tool_call_id = call
        .id
        .as_deref()
        .expect("tool call ID assigned")
        .to_string();

    let (resolved_agent, resolved_session_id) = if let Some(prepare_fn) = &ctx.prepare_handoff_fn {
        let prepared = prepare_fn(
            agent.clone(),
            caller_session_id.clone(),
            tool_call_id.clone(),
        )
        .await
        .map_err(|e| {
            // Reservation failures remain fatal per contract
            ToolError::Fatal(anyhow!("Handoff preparation failed: {}", e))
        })?;
        (prepared.agent, prepared.session_id)
    } else {
        (agent, caller_session_id.unwrap_or_default())
    };

    Ok(handoff_tool_output(&resolved_agent, &resolved_session_id, prompt, &tool_call_id).into())
}

fn handoff_tool_output(
    resolved_agent: &str,
    resolved_session_id: &str,
    prompt: &str,
    tool_call_id: &str,
) -> Value {
    // Build result per contract:
    // - Text: "Handing off to <agent>; target session: <id>." (no user-only audience)
    // - handoff marker: {agent, session_id}
    // - Agent ref consistent with HandoffCommitted.target_agent
    let text = if resolved_session_id.is_empty() {
        format!("Handing off to {}.", resolved_agent)
    } else {
        format!(
            "Handing off to {}; target session: {}.",
            resolved_agent, resolved_session_id
        )
    };

    // Build the handoff marker when we have a resolved session ID
    let handoff_marker = if resolved_session_id.is_empty() {
        None
    } else {
        Some(json!({
            "agent": resolved_agent,
            "session_id": resolved_session_id,
        }))
    };

    let mut result = json!({
        "content": [{
            "type": "text",
            "text": text,
        }],
        "action": "switch_agent",
        "agent": resolved_agent,
        "prompt": prompt,
        "session_id": if resolved_session_id.is_empty() { Value::Null } else { json!(resolved_session_id) },
        "tool_call_id": tool_call_id,
    });

    // Add handoff marker when present
    if let Some(marker) = handoff_marker {
        result["handoff"] = marker;
    }

    result
}

async fn dispatch_tool_call(
    mut call: ToolCall,
    json_data: Value,
    ctx: &ToolEvalContext,
    abort_signal: &AbortSignal,
) -> Result<ToolProviderOutput, ToolError> {
    ensure_tool_call_ids(std::slice::from_mut(&mut call));
    if call.name.ends_with("_session_handoff") {
        return dispatch_session_handoff(call, json_data, ctx).await;
    }

    let tool_call_id = call
        .id
        .clone()
        .expect("tool call ID assigned before provider dispatch");

    for provider in &ctx.providers {
        if !provider.has_tool(&call.name) {
            continue;
        }
        let tool_name = call.name.clone();
        // Create progress handle bound to the call ID and emit callback.
        let progress = crate::progress::RuntimeToolProgress::new(
            tool_call_id.clone(),
            Arc::clone(&ctx.emit_tool_update_fn),
            Arc::clone(abort_signal),
        );
        let result = call_tool_with_tracing(
            provider.as_ref(),
            &tool_name,
            json_data.clone(),
            ctx.origin(&tool_call_id),
            abort_signal,
            progress.clone(),
        )
        .await;
        // Finalize progress before returning (flushes pending state, marks terminal)
        progress.finalize();
        return result;
    }

    Err(ToolError::Recoverable(anyhow!(
        "No tool provider configured for '{}'",
        call.name
    )))
}

fn normalize_outer_result(
    evaluation: &ToolEvaluation<'_>,
    call: &ToolCall,
    completed: &mut MaterializedToolResult,
) {
    // Normalize before checkpointing as well as publication: durable results
    // must never retain a middleware-denied handoff or a rewritten call ID.
    completed.result.call = call.clone();
    if evaluation.ctx.middleware_fn.is_some()
        && (completed.result.output["is_error"] == true
            || completed.result.output["isError"] == true
            || completed.result.output["blocked_by_hook"] == true
            || completed.status != InnerToolCallStatus::Ok)
    {
        completed.result.switch_agent = None;
    }
}

#[cfg(test)]
mod tests {
    mod a2ui;
    mod inner_call;
    mod inner_context;
    mod interaction;
    mod malformed_image;
    mod middleware;

    use super::*;
    use anyhow::anyhow;
    use harnx_core::abort::create_abort_signal;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;
    use tokio::sync::Mutex;
    use tokio::time::Instant;

    struct MockToolProvider {
        tool_name: String,
        delay: Duration,
        result: Mutex<Option<Result<Value, ToolError>>>,
        panic_on_call: bool,
        call_count: Option<Arc<tokio::sync::Mutex<usize>>>,
    }

    impl MockToolProvider {
        fn ok(tool_name: &str, delay: Duration, result: Value) -> Self {
            Self {
                tool_name: tool_name.to_string(),
                delay,
                result: Mutex::new(Some(Ok(result))),
                panic_on_call: false,
                call_count: None,
            }
        }

        fn err(tool_name: &str, delay: Duration, error: ToolError) -> Self {
            Self {
                tool_name: tool_name.to_string(),
                delay,
                result: Mutex::new(Some(Err(error))),
                panic_on_call: false,
                call_count: None,
            }
        }

        fn panic(tool_name: &str) -> Self {
            Self {
                tool_name: tool_name.to_string(),
                delay: Duration::ZERO,
                result: Mutex::new(None),
                panic_on_call: true,
                call_count: None,
            }
        }

        fn ok_with_call_count(
            tool_name: &str,
            delay: Duration,
            result: Value,
            call_count: Arc<tokio::sync::Mutex<usize>>,
        ) -> Self {
            Self {
                tool_name: tool_name.to_string(),
                delay,
                result: Mutex::new(Some(Ok(result))),
                panic_on_call: false,
                call_count: Some(call_count),
            }
        }
    }

    impl ToolProvider for MockToolProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn has_tool(&self, tool_name: &str) -> bool {
            self.tool_name == tool_name
        }

        fn replay_tool_call<'a, 'b, 'c, 'async_trait>(
            &'a self,
            _replay: harnx_core::tool::ToolReplay<'b>,
            _abort: &'c AbortSignal,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Option<ToolProviderOutput>, ToolError>>
                    + Send
                    + 'async_trait,
            >,
        >
        where
            'a: 'async_trait,
            'b: 'async_trait,
            'c: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                if self.tool_name == "replay_only" {
                    self.result
                        .lock()
                        .await
                        .take()
                        .unwrap()
                        .map(|value| Some(value.into()))
                } else {
                    Ok(None)
                }
            })
        }

        fn call_tool<'life0, 'life1, 'life2, 'async_trait>(
            &'life0 self,
            tool_name: &'life1 str,
            _arguments: Value,
            _abort: &'life2 AbortSignal,
        ) -> Pin<
            Box<dyn Future<Output = Result<ToolProviderOutput, ToolError>> + Send + 'async_trait>,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                assert_eq!(tool_name, self.tool_name);
                assert!(!self.panic_on_call, "tool should not have been dispatched");
                if let Some(call_count) = &self.call_count {
                    *call_count.lock().await += 1;
                }
                tokio::time::sleep(self.delay).await;
                self.result
                    .lock()
                    .await
                    .take()
                    .expect("mock tool called more than once")
                    .map(Into::into)
            })
        }
    }

    struct CapturingToolProvider {
        tool_name: String,
        received_arguments: Arc<Mutex<Vec<Value>>>,
        result: Value,
    }

    struct PrivateContextProvider {
        observation: harnx_core::execution_context::ExecutionContextObservation,
    }

    impl ToolProvider for PrivateContextProvider {
        fn name(&self) -> &str {
            "private-context"
        }

        fn has_tool(&self, tool_name: &str) -> bool {
            tool_name == "context_tool"
        }

        fn call_tool<'life0, 'life1, 'life2, 'async_trait>(
            &'life0 self,
            _tool_name: &'life1 str,
            _arguments: Value,
            _abort: &'life2 AbortSignal,
        ) -> Pin<
            Box<dyn Future<Output = Result<ToolProviderOutput, ToolError>> + Send + 'async_trait>,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                Ok(ToolProviderOutput {
                    value: json!({"content": [{"type": "text", "text": "visible"}]}),
                    execution_context: Some(self.observation.clone()),
                    input_required: None,
                })
            })
        }
    }

    impl CapturingToolProvider {
        fn new(tool_name: &str, received_arguments: Arc<Mutex<Vec<Value>>>, result: Value) -> Self {
            Self {
                tool_name: tool_name.to_string(),
                received_arguments,
                result,
            }
        }
    }

    impl ToolProvider for CapturingToolProvider {
        fn name(&self) -> &str {
            "capturing-mock"
        }

        fn has_tool(&self, tool_name: &str) -> bool {
            self.tool_name == tool_name
        }

        fn call_tool<'life0, 'life1, 'life2, 'async_trait>(
            &'life0 self,
            tool_name: &'life1 str,
            arguments: Value,
            _abort: &'life2 AbortSignal,
        ) -> Pin<
            Box<dyn Future<Output = Result<ToolProviderOutput, ToolError>> + Send + 'async_trait>,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                assert_eq!(tool_name, self.tool_name);
                self.received_arguments.lock().await.push(arguments);
                Ok(self.result.clone().into())
            })
        }
    }

    /// The call id and round of each dispatch, in order.
    type RecordedOrigins = Arc<std::sync::Mutex<Vec<(Option<String>, Option<u64>)>>>;

    #[derive(Default)]
    struct UpdatingToolProvider {
        retained_progress: Arc<std::sync::Mutex<Option<Arc<dyn harnx_core::tool::ToolProgress>>>>,
        origins: RecordedOrigins,
    }

    impl ToolProvider for UpdatingToolProvider {
        fn name(&self) -> &str {
            "updating"
        }

        fn has_tool(&self, tool_name: &str) -> bool {
            tool_name == "updating_tool"
        }

        fn call_tool<'life0, 'life1, 'life2, 'async_trait>(
            &'life0 self,
            _tool_name: &'life1 str,
            _arguments: Value,
            _abort: &'life2 AbortSignal,
        ) -> Pin<
            Box<dyn Future<Output = Result<ToolProviderOutput, ToolError>> + Send + 'async_trait>,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async { panic!("engine must dispatch through call_tool_with_progress") })
        }

        fn call_tool_with_progress<'life0, 'life1, 'life2, 'life3, 'async_trait>(
            &'life0 self,
            tool_name: &'life1 str,
            _arguments: Value,
            origin: ToolCallOrigin<'life2>,
            _abort: &'life3 AbortSignal,
            progress: Arc<dyn harnx_core::tool::ToolProgress>,
        ) -> Pin<
            Box<dyn Future<Output = Result<ToolProviderOutput, ToolError>> + Send + 'async_trait>,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            'life3: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                assert_eq!(tool_name, "updating_tool");
                assert!(origin.tool_call_id.is_some_and(|id| !id.trim().is_empty()));
                self.origins
                    .lock()
                    .unwrap()
                    .push((origin.tool_call_id.map(str::to_string), origin.tool_round));
                *self.retained_progress.lock().unwrap() = Some(Arc::clone(&progress));
                progress.update(ToolUpdatePatch {
                    title: Some("first".to_string()),
                    ..Default::default()
                });
                progress.update(ToolUpdatePatch {
                    title: Some("second".to_string()),
                    ..Default::default()
                });
                progress.update(ToolUpdatePatch {
                    status: Some(harnx_core::event::ToolStatus::InProgress),
                    locations: Some(vec![]),
                    ..Default::default()
                });
                Ok(ToolProviderOutput::new(json!({"ok": true})))
            })
        }
    }

    /// A provider learns the round the evaluated calls belong to along with
    /// each call's id, which is what it journals the call under.
    #[tokio::test]
    async fn dispatch_tells_the_provider_each_calls_round_and_id() {
        let provider = Arc::new(UpdatingToolProvider::default());
        let origins = Arc::clone(&provider.origins);
        let mut ctx = test_context(vec![provider]);
        ctx.tool_round = Some(12);
        let call = ToolCall::new(
            "updating_tool".to_string(),
            json!({}),
            Some("model-call".to_string()),
            None,
        );
        eval_tool_calls(&ctx, vec![call], &create_abort_signal())
            .await
            .unwrap();
        assert_eq!(
            *origins.lock().unwrap(),
            [(Some("model-call".to_string()), Some(12))]
        );
    }

    #[derive(Debug)]
    enum RecordedToolEvent {
        Started(String),
        Updated(String, ToolUpdatePatch),
        Completed(String),
    }

    #[tokio::test]
    async fn private_execution_context_never_reaches_middleware_ui_or_serialization() {
        let private_path = "/private/worker/workspace";
        let mut observation = harnx_core::execution_context::ExecutionContextObservation::observe(
            std::path::Path::new(private_path),
            std::path::Path::new(private_path),
        );
        observation.workspace_root = private_path.to_string();
        observation.working_directory = private_path.to_string();
        let middleware_responses = Arc::new(std::sync::Mutex::new(Vec::new()));
        let middleware_capture = Arc::clone(&middleware_responses);
        let provider = Arc::new(PrivateContextProvider { observation });
        let mut ctx = test_context(vec![provider]);
        ctx.middleware_fn = Some(Arc::new(move |prep, _origin, next| {
            let capture = Arc::clone(&middleware_capture);
            Box::pin(async move {
                let outcome = next(prep.arguments).await;
                if let Ok(ToolMiddlewareOutcome::Completed(res)) = &outcome {
                    capture.lock().unwrap().push(res.result.output.clone());
                }
                outcome
            })
        }));
        let ui_results = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ui_capture = Arc::clone(&ui_results);
        ctx.emit_tool_result_fn = Arc::new(move |_call, value| {
            ui_capture.lock().unwrap().push(value.clone());
        });

        let results = eval_tool_calls(
            &ctx,
            vec![test_call("context_tool")],
            &create_abort_signal(),
        )
        .await
        .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].output["content"][0]["text"], "visible");
        assert_eq!(
            results[0]
                .execution_context
                .as_ref()
                .map(|context| context.workspace_root.as_str()),
            Some(private_path)
        );
        for value in middleware_responses
            .lock()
            .unwrap()
            .iter()
            .chain(ui_results.lock().unwrap().iter())
        {
            assert!(!value.to_string().contains(private_path));
        }
        assert!(!serde_json::to_string(&results[0])
            .unwrap()
            .contains(private_path));
    }

    fn test_context(providers: Vec<Arc<dyn ToolProvider>>) -> ToolEvalContext {
        test_context_with_emitters(providers, |_, _| {}, |_, _| {}, |_, _| {}, |_, _| {})
    }

    fn test_context_with_emitters(
        providers: Vec<Arc<dyn ToolProvider>>,
        emit_tool_call: impl Fn(&ToolCall, &Value) + Send + Sync + 'static,
        emit_tool_result: impl Fn(&ToolCall, &Value) + Send + Sync + 'static,
        emit_tool_blocked: impl Fn(&ToolCall, &Value) + Send + Sync + 'static,
        emit_tool_update: impl Fn(&str, &ToolUpdatePatch) + Send + Sync + 'static,
    ) -> ToolEvalContext {
        ToolEvalContext {
            on_tool_call_completed: None,
            on_interaction: None,
            middleware_fn: None,
            work_boundary: None,
            instance_id: harnx_core::instance::ServerScope::new(),
            tool_round: None,
            render: None,
            providers,
            allowed_tool_names: HashSet::new(),
            current_agent_package: None,
            handoff_targets: HashMap::new(),
            emit_tool_call_fn: Arc::new(emit_tool_call),
            emit_tool_result_fn: Arc::new(emit_tool_result),
            emit_tool_blocked_fn: Arc::new(emit_tool_blocked),
            emit_tool_update_fn: Arc::new(emit_tool_update),
            confirm_tool_use_fn: Arc::new(|_, _, _| ToolUseConfirmation::Approve),
            prepare_handoff_fn: None,
        }
    }

    #[test]
    fn tool_call_identity_is_filled_once_and_existing_ids_are_preserved() {
        let mut calls = vec![
            ToolCall::new("missing".to_string(), json!({}), None, None),
            ToolCall::new("blank".to_string(), json!({}), Some("  ".to_string()), None),
            ToolCall::new(
                "existing".to_string(),
                json!({}),
                Some("call-existing".to_string()),
                None,
            ),
        ];

        ensure_tool_call_ids(&mut calls);
        let assigned = calls[0].id.clone().expect("missing ID assigned");
        assert!(!assigned.is_empty());
        assert!(calls[1]
            .id
            .as_deref()
            .is_some_and(|id| !id.trim().is_empty()));
        assert_eq!(calls[2].id.as_deref(), Some("call-existing"));

        ensure_tool_call_ids(&mut calls);
        assert_eq!(calls[0].id.as_deref(), Some(assigned.as_str()));
        assert_eq!(calls[2].id.as_deref(), Some("call-existing"));
    }

    #[tokio::test]
    async fn emitting_provider_has_stable_identity_coalescing_and_terminal_order() {
        let provider = Arc::new(UpdatingToolProvider::default());
        let retained_progress = Arc::clone(&provider.retained_progress);
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let started = Arc::clone(&events);
        let completed = Arc::clone(&events);
        let updated = Arc::clone(&events);
        let ctx = test_context_with_emitters(
            vec![provider],
            move |call, _| {
                started.lock().unwrap().push(RecordedToolEvent::Started(
                    call.id.clone().expect("started call ID"),
                ));
            },
            move |call, _| {
                completed.lock().unwrap().push(RecordedToolEvent::Completed(
                    call.id.clone().expect("completed call ID"),
                ));
            },
            |_, _| {},
            move |id, patch| {
                updated
                    .lock()
                    .unwrap()
                    .push(RecordedToolEvent::Updated(id.to_string(), patch.clone()));
            },
        );

        let results = eval_tool_calls(
            &ctx,
            vec![ToolCall::new(
                "updating_tool".to_string(),
                json!({}),
                None,
                None,
            )],
            &create_abort_signal(),
        )
        .await
        .expect("updating provider should complete");
        assert_eq!(results.len(), 1);

        let events_guard = events.lock().unwrap();
        assert_eq!(
            events_guard.len(),
            4,
            "three patches should coalesce to two updates"
        );
        let RecordedToolEvent::Started(started_id) = &events_guard[0] else {
            panic!("first event must be Started: {:?}", events_guard[0]);
        };
        let RecordedToolEvent::Updated(first_id, first_patch) = &events_guard[1] else {
            panic!("second event must be Update: {:?}", events_guard[1]);
        };
        let RecordedToolEvent::Updated(final_id, final_patch) = &events_guard[2] else {
            panic!(
                "third event must be coalesced Update: {:?}",
                events_guard[2]
            );
        };
        let RecordedToolEvent::Completed(completed_id) = &events_guard[3] else {
            panic!("last event must be Completed: {:?}", events_guard[3]);
        };
        assert!(!started_id.is_empty());
        assert_eq!(first_id, started_id);
        assert_eq!(final_id, started_id);
        assert_eq!(completed_id, started_id);
        assert_eq!(results[0].call.id.as_deref(), Some(started_id.as_str()));
        assert_eq!(first_patch.title.as_deref(), Some("first"));
        assert_eq!(final_patch.title.as_deref(), Some("second"));
        assert!(matches!(
            final_patch.status,
            Some(harnx_core::event::ToolStatus::InProgress)
        ));
        assert_eq!(final_patch.locations, Some(vec![]));
        drop(events_guard);

        retained_progress
            .lock()
            .unwrap()
            .as_ref()
            .expect("provider retained progress")
            .update(ToolUpdatePatch {
                title: Some("late".to_string()),
                ..Default::default()
            });
        assert_eq!(
            events.lock().unwrap().len(),
            4,
            "post-terminal update emitted"
        );
    }

    #[tokio::test]
    async fn replay_queries_retired_tools_and_normalizes_null() {
        let ctx = test_context_with_emitters(
            vec![Arc::new(MockToolProvider::ok(
                "replay_only",
                Duration::ZERO,
                Value::Null,
            ))],
            |_, _| {},
            |_, value| assert!(value.is_null(), "display preserves the provider reply"),
            |_, _| {},
            |_, _| {},
        );
        let call = ToolCall::new("retired_tool".into(), json!({}), Some("call".into()), None);
        assert!(!ctx.providers[0].has_tool(&call.name));
        let recovered = replay_tool_call(
            &ctx,
            harnx_core::tool::ToolReplay {
                session_id: "session",
                tool_round: 1,
                call: &call,
                worker_id: None,
                fence_token: None,
                authorization: None,
            },
            &create_abort_signal(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(recovered.output, json!("DONE"));
    }

    fn two_tool_context(
        name_a: &str,
        delay_a: Duration,
        result_a: Value,
        name_b: &str,
        delay_b: Duration,
        result_b: Value,
    ) -> ToolEvalContext {
        test_context(vec![
            Arc::new(MockToolProvider::ok(name_a, delay_a, result_a)),
            Arc::new(MockToolProvider::ok(name_b, delay_b, result_b)),
        ])
    }

    fn test_call(name: &str) -> ToolCall {
        ToolCall::new(name.to_string(), json!({}), None, None)
    }

    #[tokio::test]
    async fn parallel_calls_run_concurrently() {
        let ctx = two_tool_context(
            "tool_a",
            Duration::from_millis(50),
            json!("a"),
            "tool_b",
            Duration::from_millis(50),
            json!("b"),
        );
        let abort_signal = create_abort_signal();

        let start = Instant::now();
        let result = eval_tool_calls(
            &ctx,
            vec![test_call("tool_a"), test_call("tool_b")],
            &abort_signal,
        )
        .await
        .expect("tool calls should succeed");
        let elapsed = start.elapsed();

        assert_eq!(result.len(), 2);
        assert!(elapsed < Duration::from_millis(80), "elapsed: {elapsed:?}");
    }

    #[tokio::test]
    async fn result_order_preserved() {
        let ctx = two_tool_context(
            "tool_a",
            Duration::from_millis(60),
            json!("slow"),
            "tool_b",
            Duration::from_millis(10),
            json!("fast"),
        );
        let abort_signal = create_abort_signal();

        let result = eval_tool_calls(
            &ctx,
            vec![test_call("tool_a"), test_call("tool_b")],
            &abort_signal,
        )
        .await
        .expect("tool calls should succeed");

        assert_eq!(result[0].output, json!("slow"));
        assert_eq!(result[1].output, json!("fast"));
    }

    #[tokio::test]
    async fn fatal_error_propagates() {
        let ctx = test_context(vec![Arc::new(MockToolProvider::err(
            "tool_a",
            Duration::ZERO,
            ToolError::Fatal(anyhow!("boom")),
        ))]);
        let abort_signal = create_abort_signal();

        let err = eval_tool_calls(&ctx, vec![test_call("tool_a")], &abort_signal)
            .await
            .expect_err("fatal error should propagate");

        assert!(format!("{err:#}").contains("boom"));
    }

    #[tokio::test]
    async fn recoverable_error_emits_result() {
        let result_emit_count = Arc::new(AtomicUsize::new(0));
        let result_emit_count_clone = Arc::clone(&result_emit_count);
        let emitted_result = Arc::new(std::sync::Mutex::new(None));
        let emitted_result_clone = Arc::clone(&emitted_result);
        let ctx = test_context_with_emitters(
            vec![Arc::new(MockToolProvider::err(
                "tool_a",
                Duration::ZERO,
                ToolError::Recoverable(anyhow!("retry")),
            ))],
            |_, _| {},
            move |_, result| {
                result_emit_count_clone.fetch_add(1, Ordering::SeqCst);
                *emitted_result_clone.lock().unwrap() = Some(result.clone());
            },
            |_, _| {},
            |_, _| {},
        );
        let abort_signal = create_abort_signal();

        let result = eval_tool_calls(&ctx, vec![test_call("tool_a")], &abort_signal)
            .await
            .expect("recoverable error should return output");

        assert_eq!(result.len(), 1);
        assert_eq!(result_emit_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            emitted_result.lock().unwrap().clone(),
            Some(json!({"is_error": true, "error": "retry"}))
        );
        assert_eq!(
            result[0].output,
            json!({"is_error": true, "error": "retry"})
        );
    }

    #[tokio::test]
    async fn a_recoverable_error_answers_with_its_partial_result() {
        let error = harnx_core::partial_result::error_with_partial_result(
            anyhow!("child failed"),
            json!({"session_id": "child-1"}),
        );
        let ctx = test_context(vec![Arc::new(MockToolProvider::err(
            "tool_a",
            Duration::ZERO,
            ToolError::Recoverable(error),
        ))]);

        let result = eval_tool_calls(&ctx, vec![test_call("tool_a")], &create_abort_signal())
            .await
            .expect("a recoverable error becomes a tool result");

        assert_eq!(
            result[0].output,
            json!({
                "is_error": true,
                "error": "child failed",
                "partial_result": {"session_id": "child-1"},
            })
        );
    }

    #[tokio::test]
    async fn missing_provider_returns_recoverable_error() {
        let ctx = test_context(Vec::new());
        let abort_signal = create_abort_signal();
        let result = eval_tool_calls(&ctx, vec![test_call("unknown_tool")], &abort_signal)
            .await
            .expect("a missing provider should become a tool result");

        let [result] = result.as_slice() else {
            panic!("missing provider should return exactly one tool result");
        };
        assert_eq!(
            (
                result.call.name.as_str(),
                result.output["is_error"].as_bool()
            ),
            ("unknown_tool", Some(true))
        );
        assert!(result.output["error"]
            .as_str()
            .expect("missing-provider error should be text")
            .contains("No tool provider configured"));
    }

    #[tokio::test]
    async fn successful_tool_result_populates_image_content_parts() {
        let image_data = "Zm9v";
        let emitted_results = Arc::new(std::sync::Mutex::new(Vec::new()));
        let emitted_results_clone = Arc::clone(&emitted_results);
        let ctx = test_context_with_emitters(
            vec![Arc::new(MockToolProvider::ok(
                "tool_a",
                Duration::ZERO,
                json!({
                    "content": [
                        {
                            "type": "text",
                            "text": "caption"
                        },
                        {
                            "type": "image",
                            "mimeType": "image/png",
                            "data": image_data
                        }
                    ]
                }),
            ))],
            |_, _| {},
            move |_, value| {
                emitted_results_clone
                    .lock()
                    .expect("lock emitted results")
                    .push(value.clone());
            },
            |_, _| {},
            |_, _| {},
        );
        let abort_signal = create_abort_signal();

        let result = eval_tool_calls(&ctx, vec![test_call("tool_a")], &abort_signal)
            .await
            .expect("tool call should succeed");

        let emitted_results = emitted_results.lock().expect("lock emitted results");
        assert_eq!(emitted_results.len(), 1);
        let emitted_output = emitted_results[0].to_string();
        assert!(!emitted_output.contains(image_data));
        assert!(emitted_output.contains("caption"));
        assert!(emitted_output.contains("<image: image/png, 4 base64 chars>"));

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content.len(), 1);
        match &result[0].content[0] {
            harnx_core::message::MessageContentPart::ImageUrl { image_url } => {
                assert_eq!(image_url.url, format!("data:image/png;base64,{image_data}"));
            }
            other => panic!("expected image content part, got {other:?}"),
        }

        let serialized_output = result[0].output.to_string();
        assert!(!serialized_output.contains(image_data));
        assert!(serialized_output.contains("caption"));
        assert!(serialized_output.contains("<image: image/png, 4 base64 chars>"));
    }

    /// Build a minimal context for handoff-resolution tests with a given
    /// active-agent package, allow-listed handoff tool name, and optional
    /// package-aware decode map.
    fn handoff_context(
        package: Option<&str>,
        allowed: &str,
        handoff_targets: HashMap<String, String>,
    ) -> ToolEvalContext {
        let mut ctx = test_context(vec![]);
        ctx.current_agent_package = package.map(str::to_string);
        ctx.allowed_tool_names = HashSet::from([allowed.to_string()]);
        ctx.handoff_targets = handoff_targets;
        ctx
    }

    async fn dispatched_handoff_agent(ctx: &ToolEvalContext, tool_name: &str) -> String {
        let call = ToolCall::new(tool_name.to_string(), json!({ "prompt": "go" }), None, None);
        let abort_signal = create_abort_signal();
        let result =
            match dispatch_tool_call(call, json!({ "prompt": "go" }), ctx, &abort_signal).await {
                Ok(result) => result,
                Err(ToolError::Recoverable(err) | ToolError::Fatal(err)) => {
                    panic!("handoff dispatch should succeed: {err:#}")
                }
            };
        result["agent"]
            .as_str()
            .expect("handoff result should carry an agent")
            .to_string()
    }

    async fn dispatched_handoff(ctx: &ToolEvalContext, tool_name: &str, arguments: Value) -> Value {
        let call = ToolCall::new(tool_name.to_string(), arguments.clone(), None, None);
        let abort_signal = create_abort_signal();
        match dispatch_tool_call(call, arguments, ctx, &abort_signal).await {
            Ok(result) => result.value,
            Err(ToolError::Recoverable(err) | ToolError::Fatal(err)) => {
                panic!("handoff dispatch should succeed: {err:#}")
            }
        }
    }

    #[test]
    fn provider_handoff_inherits_tool_call_id_when_result_omits_it() {
        let call = ToolCall::new(
            "remote_handoff".to_string(),
            json!({}),
            Some("provider-call-123".to_string()),
            None,
        );
        let result = completed_tool_result(
            call,
            json!({
                "action": "switch_agent",
                "agent": "target-agent",
                "prompt": "continue work"
            }),
            Vec::new(),
            Vec::new(),
            None,
        );

        assert_eq!(
            result
                .switch_agent
                .expect("handoff result")
                .tool_call_id
                .as_deref(),
            Some("provider-call-123")
        );
    }

    #[test]
    fn provider_handoff_keeps_result_tool_call_id() {
        let call = ToolCall::new(
            "remote_handoff".to_string(),
            json!({}),
            Some("request-call-123".to_string()),
            None,
        );
        let result = completed_tool_result(
            call,
            json!({
                "action": "switch_agent",
                "agent": "target-agent",
                "prompt": "continue work",
                "tool_call_id": "result-call-456"
            }),
            Vec::new(),
            Vec::new(),
            None,
        );

        assert_eq!(
            result
                .switch_agent
                .expect("handoff result")
                .tool_call_id
                .as_deref(),
            Some("result-call-456")
        );
    }

    #[tokio::test]
    async fn handoff_bare_target_resolves_to_current_package() {
        // #709: `pantheon/daedalus` handing off to bare `atlas` must land on
        // same-package `pantheon/atlas`, not top-level `atlas`.
        let ctx = handoff_context(Some("pantheon"), "atlas_session_handoff", HashMap::new());
        let agent = dispatched_handoff_agent(&ctx, "atlas_session_handoff").await;
        assert_eq!(agent, "pantheon/atlas");
    }

    #[tokio::test]
    async fn handoff_bare_target_top_level_unchanged() {
        // No package context (top-level agent) → bare target stays bare.
        let ctx = handoff_context(None, "atlas_session_handoff", HashMap::new());
        let agent = dispatched_handoff_agent(&ctx, "atlas_session_handoff").await;
        assert_eq!(agent, "atlas");
    }

    #[tokio::test]
    async fn handoff_leading_slash_escapes_to_top_level() {
        // `/atlas` explicitly escapes the package to top-level even when a
        // package context is present.
        let ctx = handoff_context(Some("pantheon"), "/atlas_session_handoff", HashMap::new());
        let agent = dispatched_handoff_agent(&ctx, "/atlas_session_handoff").await;
        assert_eq!(agent, "atlas");
    }

    #[tokio::test]
    async fn handoff_qualified_target_unchanged() {
        // Cross-package qualified target is left untouched.
        let ctx = handoff_context(
            Some("pantheon"),
            "other/atlas_session_handoff",
            HashMap::new(),
        );
        let agent = dispatched_handoff_agent(&ctx, "other/atlas_session_handoff").await;
        assert_eq!(agent, "other/atlas");
    }

    #[tokio::test]
    async fn handoff_uses_exact_target_map() {
        // The exact map covers same-package, cross-package, remote, and bare
        // top-level targets without applying legacy package qualification.
        for (stem, expected) in [
            ("atlas", "pantheon/atlas"),
            ("otherpkg__helper", "otherpkg/helper"),
            ("atlas__remote", "pantheon/atlas@remote"),
            ("__global", "global"),
        ] {
            let tool_name = format!("{stem}_session_handoff");
            let ctx = handoff_context(
                Some("pantheon"),
                &tool_name,
                HashMap::from([(stem.to_string(), expected.to_string())]),
            );
            assert_eq!(dispatched_handoff_agent(&ctx, &tool_name).await, expected);
        }
    }

    #[tokio::test]
    async fn handoff_legacy_fallback_still_resolves_bare_target() {
        let ctx = handoff_context(Some("pantheon"), "atlas_session_handoff", HashMap::new());
        let agent = dispatched_handoff_agent(&ctx, "atlas_session_handoff").await;
        assert_eq!(agent, "pantheon/atlas");
    }

    // ================================================================
    // Handoff preparation callback tests.
    // ================================================================

    /// Test that a prepare_handoff callback populates the handoff marker
    /// and text with the session ID.
    #[tokio::test]
    async fn handoff_with_prepare_callback_includes_marker_and_session_id() {
        use std::sync::Arc;

        // Mock prepare callback that returns a fixed session ID
        let prepared_session_id = "reserved-session-123";
        let prepared_agent = "pantheon/atlas".to_string();
        let prepared_agent_clone = prepared_agent.clone();
        let prepare_fn: Arc<PrepareHandoffFn> =
            Arc::new(move |_agent, _requested_id, _tool_call_id| {
                let agent = prepared_agent_clone.clone();
                let session_id = prepared_session_id.to_string();
                Box::pin(async move { Ok(PreparedHandoff { agent, session_id }) })
            });

        let mut ctx = handoff_context(Some("pantheon"), "atlas_session_handoff", HashMap::new());
        let mut targets = HashMap::new();
        targets.insert("atlas".to_string(), "pantheon/atlas".to_string());
        ctx.handoff_targets = targets;
        ctx.prepare_handoff_fn = Some(prepare_fn);

        let result =
            dispatched_handoff(&ctx, "atlas_session_handoff", json!({ "prompt": "go" })).await;

        // Result should have handoff marker
        assert!(
            result.get("handoff").is_some(),
            "result should have handoff marker"
        );
        let handoff = result
            .get("handoff")
            .unwrap()
            .as_object()
            .expect("handoff should be an object");
        assert_eq!(
            handoff.get("agent").and_then(|v| v.as_str()),
            Some("pantheon/atlas")
        );
        assert_eq!(
            handoff.get("session_id").and_then(|v| v.as_str()),
            Some("reserved-session-123")
        );

        // Session ID at top level should match
        assert_eq!(
            result.get("session_id").and_then(|v| v.as_str()),
            Some("reserved-session-123")
        );

        // Text should include session ID
        let content = result
            .get("content")
            .expect("content")
            .as_array()
            .expect("content array");
        assert_eq!(content.len(), 1);
        let text = content[0]
            .get("text")
            .expect("text")
            .as_str()
            .expect("text string");
        assert!(
            text.contains("target session: reserved-session-123"),
            "text should include session id: {}",
            text
        );

        // Text should NOT have user-only audience
        assert!(
            content[0].get("annotations").is_none(),
            "text should not have annotations"
        );

        // SwitchAgentData should reflect the resolved session ID
        let switch_agent = detect_switch_agent(&result).expect("handoff result");
        assert_eq!(
            switch_agent.session_id.as_deref(),
            Some("reserved-session-123")
        );
        assert_eq!(switch_agent.agent, "pantheon/atlas");
    }

    /// Test that caller-supplied session ID is passed through to prepare callback unchanged.
    #[tokio::test]
    async fn handoff_prepare_callback_receives_caller_session_id() {
        let captured_requested_id = Arc::new(std::sync::Mutex::new(None::<String>));
        let captured_clone = captured_requested_id.clone();

        let prepare_fn: Arc<PrepareHandoffFn> =
            Arc::new(move |_agent, requested_id, _tool_call_id| {
                let mut guard = captured_clone.lock().unwrap();
                *guard = requested_id.clone();
                let session_id = requested_id.unwrap_or_else(|| "fallback-id".to_string());
                Box::pin(async move {
                    Ok(PreparedHandoff {
                        agent: "pantheon/atlas".to_string(),
                        session_id,
                    })
                })
            });

        let mut ctx = handoff_context(Some("pantheon"), "atlas_session_handoff", HashMap::new());
        let mut targets = HashMap::new();
        targets.insert("atlas".to_string(), "pantheon/atlas".to_string());
        ctx.handoff_targets = targets;
        ctx.prepare_handoff_fn = Some(prepare_fn);

        let _result = dispatched_handoff(
            &ctx,
            "atlas_session_handoff",
            json!({ "prompt": "go", "session_id": "caller-session-456" }),
        )
        .await;

        // The callback should have received the caller-supplied ID
        let guard = captured_requested_id.lock().unwrap();
        assert_eq!(*guard, Some("caller-session-456".to_string()));
    }

    /// Test that without a prepare callback, the legacy behavior is preserved.
    #[tokio::test]
    async fn handoff_without_prepare_callback_uses_legacy_behavior() {
        // Legacy: session_id from caller or null
        let ctx = handoff_context(None, "atlas_session_handoff", HashMap::new());

        // With explicit session_id
        let result = dispatched_handoff(
            &ctx,
            "atlas_session_handoff",
            json!({ "prompt": "go", "session_id": "legacy-session" }),
        )
        .await;
        assert_eq!(
            result.get("session_id").and_then(|v| v.as_str()),
            Some("legacy-session")
        );

        // handoff marker should be present when session_id exists
        assert!(
            result.get("handoff").is_some(),
            "handoff marker should be present with session_id"
        );
        let handoff = result
            .get("handoff")
            .unwrap()
            .as_object()
            .expect("handoff object");
        assert_eq!(
            handoff.get("session_id").and_then(|v| v.as_str()),
            Some("legacy-session")
        );

        // Without session_id
        let result =
            dispatched_handoff(&ctx, "atlas_session_handoff", json!({ "prompt": "go" })).await;
        assert_eq!(result.get("session_id"), Some(&Value::Null));

        // handoff marker should NOT be present without session_id
        assert!(
            result.get("handoff").is_none(),
            "handoff marker should not be present without session_id"
        );
    }

    #[tokio::test]
    async fn handoff_strips_only_one_suffix() {
        // An agent literally named `worker_session_handoff` must keep its name:
        // only the trailing `_session_handoff` tool suffix is stripped, not the
        // repeated occurrence in the agent name.
        let ctx = handoff_context(
            None,
            "worker_session_handoff_session_handoff",
            HashMap::new(),
        );
        let agent = dispatched_handoff_agent(&ctx, "worker_session_handoff_session_handoff").await;
        assert_eq!(agent, "worker_session_handoff");
    }

    #[tokio::test]
    async fn handoff_propagates_prompt() {
        // The supplied prompt must flow through to the switch_agent result.
        let ctx = handoff_context(Some("pantheon"), "atlas_session_handoff", HashMap::new());
        let call = ToolCall::new(
            "atlas_session_handoff".to_string(),
            json!({ "prompt": "do the thing" }),
            None,
            None,
        );
        let abort_signal = create_abort_signal();
        let result = match dispatch_tool_call(
            call,
            json!({ "prompt": "do the thing" }),
            &ctx,
            &abort_signal,
        )
        .await
        {
            Ok(result) => result,
            Err(ToolError::Recoverable(err) | ToolError::Fatal(err)) => {
                panic!("handoff dispatch should succeed: {err:#}")
            }
        };
        assert_eq!(result["action"].as_str(), Some("switch_agent"));
        assert_eq!(result["agent"].as_str(), Some("pantheon/atlas"));
        assert_eq!(result["prompt"].as_str(), Some("do the thing"));
    }

    #[tokio::test]
    async fn handoff_normalizes_blank_session_ids_without_substituting_source_id() {
        let ctx = handoff_context(None, "atlas_session_handoff", HashMap::new());
        for arguments in [
            json!({ "prompt": "go" }),
            json!({ "prompt": "go", "session_id": "" }),
            json!({ "prompt": "go", "session_id": "  \t " }),
        ] {
            let result = dispatched_handoff(&ctx, "atlas_session_handoff", arguments.clone()).await;
            assert_eq!(result["session_id"], Value::Null, "arguments: {arguments}");
            assert_eq!(
                detect_switch_agent(&result)
                    .expect("handoff result")
                    .session_id,
                None,
                "arguments: {arguments}"
            );
        }
    }

    #[tokio::test]
    async fn handoff_preserves_explicit_session_id_exactly() {
        let ctx = handoff_context(None, "atlas_session_handoff", HashMap::new());
        let result = dispatched_handoff(
            &ctx,
            "atlas_session_handoff",
            json!({ "prompt": "go", "session_id": "atlas-existing" }),
        )
        .await;
        assert_eq!(result["session_id"].as_str(), Some("atlas-existing"));
        assert_eq!(
            detect_switch_agent(&result)
                .expect("handoff result")
                .session_id
                .as_deref(),
            Some("atlas-existing")
        );
    }

    #[tokio::test]
    async fn handoff_missing_prompt_is_recoverable_error() {
        // A handoff call without the required `prompt` argument must return a
        // Recoverable error (so the LLM can retry), not panic or switch.
        let ctx = handoff_context(Some("pantheon"), "atlas_session_handoff", HashMap::new());
        let call = ToolCall::new("atlas_session_handoff".to_string(), json!({}), None, None);
        let abort_signal = create_abort_signal();
        match dispatch_tool_call(call, json!({}), &ctx, &abort_signal).await {
            Ok(_) => panic!("missing prompt must error"),
            Err(ToolError::Recoverable(e)) => {
                assert!(e.to_string().contains("prompt"), "unexpected error: {e:#}");
            }
            Err(ToolError::Fatal(e)) => panic!("expected Recoverable, got Fatal: {e:#}"),
        }
    }

    #[tokio::test]
    async fn handoff_unallowed_tool_is_recoverable_error() {
        // A handoff tool not present in `allowed_tool_names` must be rejected
        // (no provider configured) rather than silently switching agents.
        let ctx = handoff_context(Some("pantheon"), "atlas_session_handoff", HashMap::new());
        let call = ToolCall::new(
            "unlisted_session_handoff".to_string(),
            json!({ "prompt": "go" }),
            None,
            None,
        );
        let abort_signal = create_abort_signal();
        match dispatch_tool_call(call, json!({ "prompt": "go" }), &ctx, &abort_signal).await {
            Ok(_) => panic!("unallowed handoff tool must error"),
            Err(ToolError::Recoverable(e)) => {
                assert!(
                    e.to_string().contains("unlisted_session_handoff"),
                    "unexpected error: {e:#}"
                );
            }
            Err(ToolError::Fatal(e)) => panic!("expected Recoverable, got Fatal: {e:#}"),
        }
    }
}
