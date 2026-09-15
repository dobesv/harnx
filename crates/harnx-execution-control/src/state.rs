//! Logical interruption versus physical cleanup (#1878, Stage 1).
//!
//! `LogicalState::Interrupted` is acceptance-terminal: no output or further work
//! belongs to that generation, and a new generation can start immediately.
//! `CleanupState` is a separate dimension tracking owner/child shutdown, never
//! conflated with logical stop acceptance. Use purpose-specific predicates:
//!
//! - `accepts_work()` — new admissions permitted
//! - `is_stopped()` — acceptance-terminal, eligible for generation replacement
//! - `allows_continuation()` — existing descendants may continue
//! - `can_prune()` — physical-record retirement allowed
//! - `cleanup_confirmed()` — physical shutdown complete
//!
//! Do NOT add `Interrupted` to `OperationState::is_terminal()`. That would
//! permit pruning to erase stop evidence needed for recovery/fencing.

use crate::{Operation, OperationState};
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicalState {
    Preparing,
    Running,
    /// Ended under the compatibility lifecycle, including legacy cancellation.
    Completed,
    /// Acceptance-terminal. No output or further work belongs to this generation.
    Interrupted,
}

impl LogicalState {
    pub fn accepts_work(self) -> bool {
        matches!(self, Self::Preparing | Self::Running)
    }

    pub fn can_replace_generation(self) -> bool {
        matches!(self, Self::Completed | Self::Interrupted)
    }

    pub fn is_stopped(self) -> bool {
        self.can_replace_generation()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupState {
    Pending,
    Confirmed,
    /// Cleanup has stalled or was abandoned, not a failure of stop acceptance.
    Unconfirmed,
}

impl CleanupState {
    pub fn is_cleanup_terminal(self) -> bool {
        self == Self::Confirmed
    }
}

/// Immutable receipt for one generation's accepted interruption. Persisted with
/// the logical stop in the operation CAS, not inferred from transcript progress.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopDecision {
    pub cancellation_id: String,
    pub accepted_at: DateTime<Utc>,
    pub reason: String,
}

impl Operation {
    /// Views derive from the persisted stop decision and existing lifecycle facts.
    /// Duplicating those fields would let legacy `mutate` closures leave the new
    /// dimensions stale. Stop always wins over subsequent cleanup transitions.
    pub fn logical_state(&self) -> LogicalState {
        if self.stop_decision.is_some() {
            return LogicalState::Interrupted;
        }
        match self.state {
            OperationState::Preparing => LogicalState::Preparing,
            OperationState::Completed | OperationState::Cancelled => LogicalState::Completed,
            // Legacy requests still await lifecycle completion unless the caller
            // explicitly accepts an interruption with a StopDecision.
            OperationState::Running
            | OperationState::CancelRequested
            | OperationState::Quiescing
            | OperationState::Unconfirmed => LogicalState::Running,
        }
    }

    /// Only owner/child cleanup is evidence here. Transcript coverage, including
    /// `cancel_recorded`, can delay legacy completion but cannot confirm cleanup.
    pub fn cleanup_state(&self) -> CleanupState {
        if self.abandoned || self.retired_cleanup_unconfirmed {
            CleanupState::Unconfirmed
        } else if self.owner_stopped && self.children.is_empty() {
            CleanupState::Confirmed
        } else if self.state == OperationState::Unconfirmed {
            CleanupState::Unconfirmed
        } else {
            CleanupState::Pending
        }
    }

    pub fn cleanup_confirmed(&self) -> bool {
        self.cleanup_state().is_cleanup_terminal()
    }

    pub fn is_stopped(&self) -> bool {
        self.logical_state().is_stopped()
    }

    pub fn can_replace_generation(&self) -> bool {
        self.logical_state().can_replace_generation()
    }

    /// Existing children may finish after normal admissions are sealed. An
    /// interrupted generation cannot continue, even while cleanup is pending.
    pub fn allows_continuation(&self) -> bool {
        self.logical_state().accepts_work() && self.state.accepts_work()
    }

    /// Retire physical records only at the legacy lifecycle boundary. Keep its
    /// projection/abandonment semantics until those callers migrate; logical
    /// interruption alone never permits pruning. Retirement retains stop evidence.
    pub fn can_prune(&self) -> bool {
        self.state.is_lifecycle_terminal()
    }

    /// Accept once. Retries, even with another ID/reason, retain the first receipt.
    /// This only changes the model; `ExecutionStore::accept_interrupt` persists it.
    ///
    /// Legacy `request_cancel` intentionally does not call this yet: it keeps the
    /// old blocking behavior for callers that haven't opted into gate-native output.
    /// Mixing Stage 1 per-node acceptance with gate output commits would reintroduce
    /// TOCTOU races without a single CAS anchor.
    pub fn accept_interrupt(&mut self, decision: StopDecision) -> Result<()> {
        if self.stop_decision.is_some() {
            return Ok(());
        }
        ensure!(!self.is_stopped(), "execution already finished");
        ensure!(
            !decision.cancellation_id.is_empty(),
            "interruption requires a cancellation id"
        );
        self.request_cancel(&decision.cancellation_id, false)?;
        self.stop_decision = Some(decision);
        Ok(())
    }

    /// Public mutation closures must not erase or rewrite accepted stop evidence.
    pub(crate) fn check_stop_decision(&self, previous: Option<&StopDecision>) -> Result<()> {
        if let Some(previous) = previous {
            ensure!(
                self.stop_decision.as_ref() == Some(previous),
                "accepted stop decision cannot change"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
