//! One CAS document owns the active snapshot and all work identities.
use super::super::{parse_task_id, TaskRecord};
use a2a_lf::StreamResponse;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerFence {
    pub boot_id: String,
    pub epoch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextDocument {
    pub version: u32,
    pub local_id: String,
    pub epoch: u64,
    pub owner: Option<OwnerFence>,
    /// Acquisition revision, not the moving renewal revision of the lease.
    pub last_lease_revision: u64,
    pub state: ContextState,
    pub last_operation: OperationReceipt,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextState {
    pub active: Option<ActiveTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveTask {
    pub snapshot: TaskRecord,
    pub message: RetainedMessage,
    pub admission: AdmissionState,
    #[serde(default)]
    pub cancel: Option<CancelIntent>,
    #[serde(default)]
    pub publication: PublicationState,
    #[serde(default)]
    pub projections: TerminalProjections,
    /// Set only after a durable scoped stop or non-executable closure, not HTTP disconnect.
    #[serde(default)]
    pub stop_confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedMessage {
    pub message_id: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionState {
    pub invocation_id: String,
    pub prompt_id: String,
    pub fixed_predecessor: u64,
    pub phase: AdmissionPhase,
    #[serde(default)]
    pub prompt_sequence: Option<u64>,
    #[serde(default)]
    pub closure_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionPhase {
    Reserved,
    Admitted,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelIntent {
    #[serde(default)]
    pub requested_at: Option<chrono::DateTime<chrono::Utc>>,
    pub operation_id: String,
    pub task_id: String,
    pub invocation_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PublicationState {
    #[serde(default)]
    pub stream_seq: u64,
    #[serde(default)]
    pub subject_sequence: u64,
    #[serde(default)]
    pub pending: Option<PendingEvent>,
    /// Confirmed sequence cutoffs. Purge by this frozen floor, never by a live tail.
    #[serde(default)]
    pub confirmed_history: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingEvent {
    #[serde(default)]
    pub committed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub commit_id: String,
    pub task_sequence: u64,
    pub expected_subject_sequence: u64,
    pub response: StreamResponse,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TerminalProjections {
    pub archive: bool,
    pub message_mapping: bool,
    pub final_event: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationReceipt {
    pub id: String,
    pub digest: String,
    pub predecessor: u64,
}

impl ActiveTask {
    pub(crate) fn ready_to_retire(&self) -> bool {
        self.snapshot.task.status.state.is_terminal()
            && self.stop_confirmed
            && self.projections.archive
            && self.projections.message_mapping
            && self.projections.final_event
            && self.publication.pending.is_none()
    }

    pub(super) fn validate(&self, local_id: &str) -> Result<()> {
        ensure!(
            self.snapshot.version == 1,
            "unsupported task record version"
        );
        ensure!(
            self.snapshot.task.context_id == local_id
                && parse_task_id(&self.snapshot.task.id)?.0 == local_id,
            "task context mismatch"
        );
        ensure!(
            !self.message.message_id.is_empty() && !self.message.fingerprint.is_empty(),
            "missing message identity"
        );
        ensure!(
            !self.admission.invocation_id.is_empty() && !self.admission.prompt_id.is_empty(),
            "missing admission identity"
        );
        if let Some(cancel) = &self.cancel {
            ensure!(
                cancel.task_id == self.snapshot.task.id
                    && cancel.invocation_id == self.admission.invocation_id
                    && !cancel.operation_id.is_empty(),
                "cancel intent identity mismatch"
            );
        }
        self.publication.validate()?;
        ensure!(
            self.snapshot.stream_seq == self.publication.stream_seq,
            "snapshot/publication cursor mismatch"
        );
        Ok(())
    }
}

impl ContextDocument {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported context authority version");
        ensure!(
            self.epoch > 0 && self.last_lease_revision > 0,
            "invalid authority epoch or lease revision"
        );
        if let Some(owner) = &self.owner {
            ensure!(
                owner.epoch == self.epoch && !owner.boot_id.is_empty(),
                "invalid owner fence"
            );
        }
        if let Some(active) = &self.state.active {
            active.validate(&self.local_id)?;
        }
        Ok(())
    }
}

impl PublicationState {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.confirmed_history.len() <= harnx_runtime::a2a_events::CHECKPOINT_HISTORY
                && self
                    .confirmed_history
                    .iter()
                    .all(|seq| *seq > 0 && *seq <= self.subject_sequence)
                && self
                    .confirmed_history
                    .windows(2)
                    .all(|pair| pair[0] < pair[1]),
            "invalid confirmed event history"
        );
        if let Some(pending) = &self.pending {
            ensure!(
                pending.task_sequence > 0 && !pending.commit_id.is_empty(),
                "invalid pending event identity"
            );
            ensure!(
                pending.task_sequence == self.stream_seq,
                "pending event cursor mismatch"
            );
        }
        Ok(())
    }
}
