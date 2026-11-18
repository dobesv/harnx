//! Owned round-state conversion and fatal-error context.

use super::{
    EvaluatedInnerCall, EvaluatedRound, ToolInputRequiredError, ToolRoundFailure, ToolRoundOutcome,
    ToolRoundSuspension,
};
use anyhow::Result;

impl EvaluatedRound {
    pub(super) fn suspension(&self) -> ToolRoundSuspension {
        ToolRoundSuspension {
            completed: self
                .completed
                .iter()
                .map(|call| call.result.clone())
                .collect(),
            pending: self.pending.clone(),
        }
    }

    pub(super) fn with_failure(&self, error: anyhow::Error) -> anyhow::Error {
        // No sibling state to attach. Keep legacy alternate error formatting intact.
        if self.completed.is_empty() && self.pending.is_empty() {
            return error;
        }
        let message = error.to_string();
        let suspension = self.suspension();
        error.context(ToolRoundFailure {
            completed: suspension.completed,
            pending: suspension.pending,
            message,
        })
    }

    pub(super) fn into_outcome(self) -> ToolRoundOutcome {
        let completed = self.completed.into_iter().map(|call| call.result).collect();
        if self.pending.is_empty() {
            ToolRoundOutcome::Completed(completed)
        } else {
            ToolRoundOutcome::InputRequired(ToolRoundSuspension {
                completed,
                pending: self.pending,
            })
        }
    }

    pub(in crate::tool) fn into_legacy(self, lossless: bool) -> Result<Vec<EvaluatedInnerCall>> {
        if !self.pending.is_empty() {
            return Err(ToolInputRequiredError(self.suspension()).into());
        }
        Ok(if self.all_null && !lossless {
            vec![]
        } else {
            self.completed
        })
    }
}
