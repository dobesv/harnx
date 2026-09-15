use crate::StopDecision;
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const BUCKET: &str = "harnx_execution_control";
pub const UNCONFIRMED_AFTER_MS: u64 = 5_000;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OperationRef {
    pub session_id: String,
    pub execution_id: String,
}

impl OperationRef {
    pub fn new(session_id: impl Into<String>, execution_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            execution_id: execution_id.into(),
        }
    }

    pub fn key(&self) -> String {
        format!(
            "sessions/{}/operations/{}",
            self.session_id, self.execution_id
        )
    }

    pub fn validate(&self) -> Result<()> {
        for part in [&self.session_id, &self.execution_id] {
            ensure!(
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "invalid execution identity"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Session,
    Tool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Preparing,
    Running,
    CancelRequested,
    Quiescing,
    Unconfirmed,
    Completed,
    Cancelled,
}

impl OperationState {
    /// Compatibility alias for legacy lifecycle completion.
    ///
    /// Deliberately does NOT include `Interrupted`. Use purpose-specific
    /// predicates instead: `accepts_work()` for new admissions, `cancelling()`
    /// for in-flight cancellation, `is_lifecycle_terminal()` for physical-record
    /// retirement. See [`state.rs`] for logical interruption semantics.
    pub fn is_terminal(self) -> bool {
        self.is_lifecycle_terminal()
    }

    /// Legacy lifecycle completion, including the explicit abandonment override.
    /// This is not proof of physical cleanup; use `Operation::cleanup_confirmed`.
    pub fn is_lifecycle_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }
    pub fn accepts_work(self) -> bool {
        matches!(self, Self::Preparing | Self::Running)
    }
    pub fn cancelling(self) -> bool {
        !self.accepts_work() && !self.is_lifecycle_terminal()
    }

    pub fn can_transition(self, next: Self) -> bool {
        use OperationState::*;
        matches!(
            (self, next),
            (Preparing, Running | CancelRequested)
                | (Running, CancelRequested | Completed)
                | (CancelRequested, Quiescing | Unconfirmed)
                | (Quiescing, Cancelled | Unconfirmed)
                | (Unconfirmed, CancelRequested | Cancelled)
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub instance_id: String,
    /// Initial lease revision, not the revision advanced by renewal.
    pub fence: u64,
}

impl Owner {
    /// Tool and hook owners have no session lease. A fresh identity prevents a
    /// replacement server with the same routing name from taking over live work.
    pub fn invocation(server: &str) -> Self {
        Self {
            instance_id: format!("{server}:{}", uuid::Uuid::now_v7()),
            fence: 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cancellation {
    pub cancellation_id: String,
    pub requested_at: DateTime<Utc>,
    pub progress_at: DateTime<Utc>,
    pub attempt: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Operation {
    pub reference: OperationRef,
    pub kind: OperationKind,
    pub parent: Option<OperationRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_generation: Option<OperationRef>,
    pub children: BTreeSet<OperationRef>,
    pub owner: Option<Owner>,
    /// Historical worker fences bind pre-gate transcript entries across handover.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub owner_fences: Box<BTreeSet<u64>>,
    pub state: OperationState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub cancellation: Option<Cancellation>,
    /// Accepted logical stop, independent of cleanup and transcript projection.
    /// Once present, store mutations cannot clear or replace it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_decision: Option<StopDecision>,
    /// Activation marker, serialized against legacy cancellation before opening the gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_registration: Option<Box<crate::gate::GateRegistration>>,
    /// None means append is reserved but has not yet been acknowledged.
    pub admissions: BTreeMap<String, Option<u64>>,
    pub blocker: Option<String>,
    /// Set only by the owner after per-call cleanup and lease release.
    pub owner_stopped: bool,
    /// A retired descendant lacked physical cleanup confirmation. Dropping its
    /// graph edge must not make this operation report confirmed shutdown.
    #[serde(default)]
    pub retired_cleanup_unconfirmed: bool,
    /// CAS-sealed by an owner once its admission high-water mark is drained.
    pub sealed: bool,
    pub covered_through: u64,
    /// Transcript cancellation coverage has been projected (or no projection is
    /// required for never-started/abandoned work under the compatibility path).
    /// This is neither stop acceptance nor evidence of physical cleanup.
    pub cancel_recorded: bool,
    /// An operator explicitly made an unconfirmed cancellation terminal.
    /// The abandoned owner may still be running outside the control plane.
    #[serde(default)]
    pub abandoned: bool,
}

impl Operation {
    pub fn preparing(
        reference: OperationRef,
        kind: OperationKind,
        parent: Option<OperationRef>,
    ) -> Self {
        Self {
            reference,
            kind,
            parent,
            previous_generation: None,
            children: BTreeSet::new(),
            owner: None,
            owner_fences: Box::default(),
            state: OperationState::Preparing,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            cancellation: None,
            stop_decision: None,
            gate_registration: None,
            admissions: BTreeMap::new(),
            blocker: None,
            owner_stopped: false,
            retired_cleanup_unconfirmed: false,
            sealed: false,
            covered_through: 0,
            cancel_recorded: false,
            abandoned: false,
        }
    }

    pub(crate) fn session_generation(
        reference: OperationRef,
        parent: Option<OperationRef>,
        previous_generation: Option<OperationRef>,
    ) -> Self {
        let mut operation = Self::preparing(reference, OperationKind::Session, parent);
        operation.previous_generation = previous_generation;
        operation
    }

    pub fn transition(&mut self, state: OperationState) -> Result<()> {
        ensure!(
            self.state.can_transition(state),
            "illegal execution transition {:?} -> {:?}",
            self.state,
            state
        );
        self.state = state;
        self.updated_at = Utc::now();
        if let Some(cancel) = self.cancellation.as_mut() {
            cancel.progress_at = self.updated_at;
        }
        Ok(())
    }

    pub fn request_cancel(&mut self, cancellation_id: &str, retry: bool) -> Result<()> {
        if self.state.is_lifecycle_terminal() {
            return Ok(());
        }
        let now = Utc::now();
        if self.should_request_cancel(retry) {
            self.transition(OperationState::CancelRequested)?;
        }
        match self.cancellation.as_mut() {
            Some(cancel) if retry => {
                cancel.attempt += 1;
                cancel.progress_at = now;
            }
            Some(_) => {}
            None => {
                self.cancellation = Some(Cancellation {
                    cancellation_id: cancellation_id.into(),
                    requested_at: now,
                    progress_at: now,
                    attempt: 1,
                })
            }
        }
        Ok(())
    }

    fn should_request_cancel(&self, retry: bool) -> bool {
        if self.state == OperationState::Unconfirmed {
            return retry;
        }
        self.state.accepts_work()
    }

    pub fn accepts_work(&self) -> bool {
        self.allows_continuation() && !self.sealed && !self.owner_stopped
    }

    pub fn admissions_covered(&self, through: u64) -> bool {
        self.admissions
            .values()
            .all(|seq| seq.is_some_and(|seq| seq <= through))
    }

    pub(crate) fn reconcile_completion(&mut self) -> Result<()> {
        if !self.owner_stopped || !self.children.is_empty() {
            return Ok(());
        }
        if !self.admissions_covered(self.covered_through) {
            return Ok(());
        }
        if self.state == OperationState::Running {
            return self.transition(OperationState::Completed);
        }
        // Legacy lifecycle/pruning still waits for transcript projection. The
        // independent cleanup view can already be Confirmed at this point.
        if self.kind == OperationKind::Session && !self.cancel_recorded {
            return Ok(());
        }
        if self.state == OperationState::CancelRequested {
            self.transition(OperationState::Quiescing)?;
        }
        if matches!(
            self.state,
            OperationState::Quiescing | OperationState::Unconfirmed
        ) {
            self.transition(OperationState::Cancelled)?;
        }
        Ok(())
    }

    pub(crate) fn expire_progress(&mut self) -> Result<()> {
        if !matches!(
            self.state,
            OperationState::CancelRequested | OperationState::Quiescing
        ) {
            return Ok(());
        }
        let expired = self.cancellation.as_ref().is_some_and(|cancel| {
            (Utc::now() - cancel.progress_at).num_milliseconds() >= UNCONFIRMED_AFTER_MS as i64
        });
        if expired {
            self.transition(OperationState::Unconfirmed)?;
            self.blocker = Some(
                "execution owner, prompt admission, or registered child has not stopped".into(),
            );
        }
        Ok(())
    }

    pub(crate) fn abandon_unconfirmed(&mut self) -> Result<()> {
        if self.state.is_lifecycle_terminal() {
            return Ok(());
        }
        ensure!(
            self.state.cancelling(),
            "only a cancelling execution can be abandoned"
        );
        if self.state == OperationState::CancelRequested {
            self.transition(OperationState::Quiescing)?;
        }
        self.transition(OperationState::Cancelled)?;
        self.owner_stopped = true;
        self.sealed = true;
        self.cancel_recorded = true;
        self.abandoned = true;
        self.children.clear();
        self.blocker = Some("cancellation abandoned by operator; prior work may still run".into());
        Ok(())
    }

    pub fn check_owner(&self, owner: &Owner) -> Result<()> {
        ensure!(!self.abandoned, "execution was abandoned by operator");
        ensure!(
            self.owner.as_ref() == Some(owner),
            "execution owner fence changed"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CancelRequest {
    pub expected_execution_id: Option<String>,
    #[serde(default)]
    pub retry: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelDisposition {
    Idle,
    Requested,
    AlreadyRequested,
    Quiescing,
    Cancelled,
    Unconfirmed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelReceipt {
    /// Compatibility field: true means cancellation was accepted, not stopped.
    pub cancelled: bool,
    pub disposition: CancelDisposition,
    pub cancellation_id: Option<String>,
    pub execution_id: Option<String>,
    pub requested_at: Option<DateTime<Utc>>,
    pub unconfirmed_after_ms: u64,
    #[serde(default)]
    pub abandoned: bool,
}

impl CancelReceipt {
    pub fn idle() -> Self {
        Self {
            cancelled: false,
            disposition: CancelDisposition::Idle,
            cancellation_id: None,
            execution_id: None,
            requested_at: None,
            unconfirmed_after_ms: UNCONFIRMED_AFTER_MS,
            abandoned: false,
        }
    }

    pub fn from_operation(operation: &Operation, already_requested: bool) -> Self {
        use OperationState::*;
        let disposition = match operation.state {
            CancelRequested if already_requested => CancelDisposition::AlreadyRequested,
            CancelRequested => CancelDisposition::Requested,
            Quiescing => CancelDisposition::Quiescing,
            Unconfirmed => CancelDisposition::Unconfirmed,
            Cancelled => CancelDisposition::Cancelled,
            _ => CancelDisposition::Idle,
        };
        Self {
            cancelled: operation.cancellation.is_some(),
            disposition,
            cancellation_id: operation
                .cancellation
                .as_ref()
                .map(|c| c.cancellation_id.clone()),
            execution_id: Some(operation.reference.execution_id.clone()),
            requested_at: operation.cancellation.as_ref().map(|c| c.requested_at),
            unconfirmed_after_ms: UNCONFIRMED_AFTER_MS,
            abandoned: operation.abandoned,
        }
    }
}

pub type CancellationStatus = CancelReceipt;
