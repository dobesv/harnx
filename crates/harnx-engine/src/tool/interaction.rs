//! Round outcomes and sibling-safe outer completion. No persistence lives here.

use super::{
    dispatch_authorized_call, ensure_tool_call_ids, materialize_dispatch_result,
    normalize_outer_result, prepare_tool_call, publish_tool_call, run_middleware, work_boundary,
    ApprovedToolCall, EvaluatedInnerCall, InnerToolCallStatus, MaterializedToolResult,
    ToolAdmission, ToolApprovalRequiredError, ToolEvalContext, ToolEvaluation,
    ToolMiddlewareOrigin, ToolMiddlewareOutcome, WorkBoundary,
};
use anyhow::{anyhow, Result};
use futures_util::future::join_all;
use harnx_core::{
    abort::AbortSignal,
    tool::{ToolCall, ToolError, ToolResult},
};
use serde_json::json;
use std::collections::HashMap;

mod state;
mod types;
pub use types::{
    PendingToolInteraction, ToolCallCheckpoint, ToolCallCompletedFn, ToolInputRequiredError,
    ToolInteractionFn, ToolRoundFailure, ToolRoundOutcome, ToolRoundSuspension,
};

/// Evaluate a model round, reusing supplied durable outer completions. Reused
/// calls bypass preparation, approval, middleware, dispatch and publication.
/// The caller must restore private snapshots before supplying completed results.
pub async fn eval_tool_round(
    ctx: &ToolEvalContext,
    calls: Vec<ToolCall>,
    completed: Vec<ToolResult>,
    abort: &AbortSignal,
) -> Result<ToolRoundOutcome> {
    eval_tool_round_with_authorization(ctx, calls, completed, abort, None).await
}

pub async fn eval_tool_round_with_authorization(
    ctx: &ToolEvalContext,
    calls: Vec<ToolCall>,
    completed: Vec<ToolResult>,
    abort_signal: &AbortSignal,
    authorization: Option<&dyn harnx_core::tool::ReplayAuthorization>,
) -> Result<ToolRoundOutcome> {
    let round = evaluate_round(
        ToolEvaluation {
            ctx,
            abort_signal,
            authorization,
            lossless: false,
            origin: ToolMiddlewareOrigin::Model,
        },
        calls,
        completed,
    )
    .await?;
    Ok(round.into_outcome())
}

pub(super) struct EvaluatedRound {
    completed: Vec<EvaluatedInnerCall>,
    pending: Vec<PendingToolInteraction>,
    all_null: bool,
}

fn take_completed(
    calls: &[ToolCall],
    completed: Vec<ToolResult>,
) -> Result<HashMap<String, ToolResult>> {
    let mut by_id = HashMap::new();
    for result in completed {
        let id = result
            .call
            .id
            .clone()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("supplied completed call has no identity"))?;
        anyhow::ensure!(
            calls.contains(&result.call),
            "supplied completed call '{id}' does not match this round"
        );
        anyhow::ensure!(
            by_id.insert(id.clone(), result).is_none(),
            "duplicate supplied completed call '{id}'"
        );
    }
    Ok(by_id)
}

pub(super) async fn evaluate_round(
    evaluation: ToolEvaluation<'_>,
    mut calls: Vec<ToolCall>,
    completed: Vec<ToolResult>,
) -> Result<EvaluatedRound> {
    ensure_tool_call_ids(&mut calls);
    let calls = ToolCall::dedup(calls);
    let mut completed = take_completed(&calls, completed)?;
    let mut round = EvaluatedRound {
        completed: vec![],
        pending: vec![],
        all_null: true,
    };
    let mut approved = Vec::new();
    let mut deferred = Vec::new();
    for call in &calls {
        if let Some(result) = call.id.as_ref().and_then(|id| completed.remove(id)) {
            round.all_null = false;
            round.completed.push(EvaluatedInnerCall {
                result,
                status: InnerToolCallStatus::Ok,
            });
            continue;
        }
        match prepare_tool_call(evaluation.ctx, call.clone(), evaluation.abort_signal).await? {
            ToolAdmission::Approved(call) => approved.push(call),
            ToolAdmission::Immediate(result, status) => {
                let result = finalize_immediate(evaluation, *result, status).await?;
                round.completed.push(EvaluatedInnerCall { result, status });
                round.all_null = false;
            }
            ToolAdmission::Deferred(call) => deferred.push(call),
        }
    }
    if !deferred.is_empty() {
        return Err(ToolApprovalRequiredError::new(calls, deferred).into());
    }
    // Finalization lives inside each future, not after join_all. A slow sibling
    // must not delay another call's durable completion or terminal publication.
    let futures = approved
        .iter()
        .map(|call| finalize_dispatch(evaluation, call));
    let dispatched = join_all(futures).await;
    collect_dispatch_results(evaluation, &calls, &mut round, dispatched).await?;
    Ok(round)
}

async fn finalize_immediate(
    evaluation: ToolEvaluation<'_>,
    result: ToolResult,
    status: InnerToolCallStatus,
) -> Result<ToolResult> {
    let result = complete_result(evaluation.ctx, result).await?;
    if status == InnerToolCallStatus::Blocked {
        (evaluation.ctx.emit_tool_blocked_fn)(&result.call, &result.output);
    }
    Ok(result)
}

async fn complete_result(ctx: &ToolEvalContext, result: ToolResult) -> Result<ToolResult> {
    match &ctx.on_tool_call_completed {
        Some(complete) => complete(result).await,
        None => Ok(result),
    }
}

enum FinalizedCall {
    Completed(Box<EvaluatedInnerCall>, bool),
    InputRequired(Box<PendingToolInteraction>),
}

// qual:allow(iosp) — Select the installed middleware or leaf dispatcher; both return the same typed outcome.
async fn dispatch_outcome(
    evaluation: ToolEvaluation<'_>,
    call: &ApprovedToolCall,
) -> Result<ToolMiddlewareOutcome> {
    let outcome = if let Some(middleware) = &evaluation.ctx.middleware_fn {
        run_middleware(evaluation, call, middleware.as_ref()).await
    } else {
        let output = dispatch_authorized_call(
            call,
            evaluation.ctx,
            evaluation.abort_signal,
            evaluation.authorization,
        )
        .await;
        materialize_dispatch_result(&evaluation, call, output)
            .await
            .map(ToolMiddlewareOutcome::Completed)
    };
    outcome.map_err(|error| match error {
        ToolError::Fatal(error) | ToolError::Recoverable(error) => error,
    })
}

fn supports_interaction(evaluation: ToolEvaluation<'_>) -> bool {
    evaluation.ctx.on_interaction.is_some() && evaluation.origin == ToolMiddlewareOrigin::Model
}

// qual:allow(iosp) — Route typed completion/suspension; unsupported callers must finalize an error instead of parking.
async fn finalize_dispatch(
    evaluation: ToolEvaluation<'_>,
    call: &ApprovedToolCall,
) -> Result<FinalizedCall> {
    match dispatch_outcome(evaluation, call).await? {
        ToolMiddlewareOutcome::Completed(result) => {
            finalize_materialized(evaluation, call, result).await
        }
        ToolMiddlewareOutcome::InputRequired {
            interaction,
            checkpoint,
        } => {
            if supports_interaction(evaluation) {
                Ok(FinalizedCall::InputRequired(Box::new(
                    PendingToolInteraction {
                        call: call.call.clone(),
                        interaction,
                        checkpoint,
                    },
                )))
            } else {
                let result = MaterializedToolResult::new(
                    ToolResult::new(
                        call.call.clone(),
                        json!({"is_error": true, "error": "interaction not supported here"}),
                    ),
                    InnerToolCallStatus::ToolError,
                );
                finalize_materialized(evaluation, call, result).await
            }
        }
    }
}

async fn finalize_materialized(
    evaluation: ToolEvaluation<'_>,
    call: &ApprovedToolCall,
    mut completed: MaterializedToolResult,
) -> Result<FinalizedCall> {
    if evaluation.ctx.middleware_fn.is_some() {
        work_boundary(
            evaluation.ctx,
            &call.call,
            evaluation.abort_signal,
            WorkBoundary::Accept,
        )
        .await?;
    }
    normalize_outer_result(&evaluation, &call.call, &mut completed);
    let was_null = completed.was_null();
    completed.result = complete_result(evaluation.ctx, completed.result).await?;
    Ok(FinalizedCall::Completed(
        Box::new(publish_tool_call(&evaluation, call, completed)),
        was_null,
    ))
}

async fn collect_dispatch_results(
    evaluation: ToolEvaluation<'_>,
    calls: &[ToolCall],
    round: &mut EvaluatedRound,
    dispatched: Vec<Result<FinalizedCall>>,
) -> Result<()> {
    let mut fatal = None;
    for result in dispatched {
        match result {
            Ok(FinalizedCall::Completed(call, was_null)) => {
                round.all_null &= was_null;
                round.completed.push(*call);
            }
            Ok(FinalizedCall::InputRequired(pending)) => round.pending.push(*pending),
            Err(error) if fatal.is_none() => fatal = Some(error),
            Err(_) => {}
        }
    }
    round.completed.sort_by_key(|result| {
        calls
            .iter()
            .position(|call| call.id == result.result.call.id)
    });
    if let Some(error) = fatal {
        return Err(round.with_failure(error));
    }
    if !round.pending.is_empty() {
        if let Some(interact) = &evaluation.ctx.on_interaction {
            interact(round.suspension())
                .await
                .map_err(|error| round.with_failure(error))?;
        }
    }
    Ok(())
}
