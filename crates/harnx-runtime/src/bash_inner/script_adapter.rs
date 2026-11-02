//! Runtime admission behind harnx-script's transport-free executor interface.

use std::sync::Arc;

use harnx_core::abort::AbortSignal;
use harnx_script::{
    commands::{CallOutcome, InnerExecutor, RejectionKind},
    CancellationToken,
};
use serde_json::Value;

use super::{InnerCallRun, InnerOutcome};

/// One adapter per script invocation. Construct `run` from the real outer eval
/// context at invocation time, not while building providers. Keep an Arc to the
/// same run for the final trace. Neither cancellation nor completion resets or
/// aborts the caller's signal.
pub struct ScriptInnerExecutor {
    run: Arc<InnerCallRun>,
    abort: AbortSignal,
}

impl ScriptInnerExecutor {
    pub fn new(run: Arc<InnerCallRun>, abort: AbortSignal) -> Self {
        Self { run, abort }
    }
}

#[async_trait::async_trait]
impl InnerExecutor for ScriptInnerExecutor {
    async fn call(&self, name: &str, args: Value, cancel: CancellationToken) -> CallOutcome {
        // Token cancellation also covers semaphore waiters. Parent abort goes
        // through the evaluator/provider, preserving their abort handling rather
        // than racing it with a second wait here. Dropping call() records a
        // Cancelled trace via its guard.
        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => return CallOutcome::Fatal { message: "script command cancelled".into() },
            outcome = self.run.call(name, args, &self.abort) => outcome,
        };
        match outcome {
            InnerOutcome::Ok { result } => CallOutcome::Ok(result.output),
            InnerOutcome::ToolError {
                message,
                partial_result,
            } => CallOutcome::ToolError {
                message,
                partial_result,
            },
            InnerOutcome::ApprovalRequired { .. } => CallOutcome::ApprovalRequired,
            InnerOutcome::Blocked { reason } => CallOutcome::Blocked { reason },
            InnerOutcome::Rejected { reason } => CallOutcome::Rejected {
                reason,
                kind: if self.run.allowed_tools().contains(name) {
                    RejectionKind::Other
                } else {
                    RejectionKind::UnknownCommand
                },
            },
            InnerOutcome::Fatal { message } => CallOutcome::Fatal { message },
        }
    }
}

#[cfg(test)]
mod tests;
