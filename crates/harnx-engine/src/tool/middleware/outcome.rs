//! Normalize leaf dispatches and recoverable middleware failures without publication.

use super::{MaterializedToolResult, ToolMiddlewareOutcome};
use crate::tool::{
    materialize_failed_tool_call, materialize_successful_tool_call, work_boundary,
    ApprovedToolCall, InnerToolCallStatus, ToolEvaluation, WorkBoundary,
};
use harnx_core::{
    partial_result::{output_with_partial_result, partial_result_of},
    tool::{ToolError, ToolProviderOutput, ToolResult},
};
use serde_json::json;

pub(super) fn finish_middleware(
    approved: &ApprovedToolCall,
    outcome: Result<ToolMiddlewareOutcome, ToolError>,
) -> Result<ToolMiddlewareOutcome, ToolError> {
    match outcome {
        Ok(outcome) => Ok(outcome),
        Err(ToolError::Recoverable(error)) => Ok(ToolMiddlewareOutcome::Completed(
            MaterializedToolResult::new(
                ToolResult::new(
                    approved.call.clone(),
                    output_with_partial_result(
                        json!({"is_error": true, "error": format!("{error:#}")}),
                        partial_result_of(&error),
                    ),
                ),
                InnerToolCallStatus::ToolError,
            ),
        )),
        Err(error @ ToolError::Fatal(_)) => Err(error),
    }
}

pub(in crate::tool) async fn materialize_dispatch_result(
    evaluation: &ToolEvaluation<'_>,
    approved: &ApprovedToolCall,
    output: Result<ToolProviderOutput, ToolError>,
) -> Result<MaterializedToolResult, ToolError> {
    if let Err(error @ ToolError::Fatal(_)) = output {
        return Err(error);
    }
    work_boundary(
        evaluation.ctx,
        &approved.call,
        evaluation.abort_signal,
        WorkBoundary::Accept,
    )
    .await
    .map_err(ToolError::Fatal)?;
    match output {
        Ok(output) => {
            let (result, was_null) = materialize_successful_tool_call(evaluation, approved, output)
                .await
                .map_err(ToolError::Fatal)?;
            Ok(MaterializedToolResult {
                result,
                status: InnerToolCallStatus::Ok,
                normalized_null: was_null && !evaluation.lossless,
            })
        }
        Err(ToolError::Recoverable(error)) => {
            let result = materialize_failed_tool_call(
                evaluation.ctx,
                approved,
                error,
                evaluation.abort_signal,
            )
            .await
            .map_err(ToolError::Fatal)?;
            Ok(MaterializedToolResult::new(
                result,
                InnerToolCallStatus::ToolError,
            ))
        }
        Err(error @ ToolError::Fatal(_)) => Err(error),
    }
}
