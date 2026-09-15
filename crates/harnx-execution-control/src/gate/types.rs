use crate::{CleanupStatus, OperationKind, OperationRef, Owner, StopDecision};
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Identity of one writer attempt, captured when the operation/sink is created.
/// Deserialize across transports, but never replace it with the current owner
/// or generation when a delayed result arrives. The gate checks every field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionContext {
    pub(crate) generation: OperationRef,
    pub(crate) gate_root: OperationRef,
    pub(crate) operation: OperationRef,
    pub(crate) owner: Owner,
    pub(crate) generation_owner: Owner,
}

impl ExecutionContext {
    pub fn new(
        generation: OperationRef,
        gate_root: OperationRef,
        operation: OperationRef,
        (owner, generation_owner): (Owner, Owner),
    ) -> Self {
        Self {
            generation,
            gate_root,
            operation,
            owner,
            generation_owner,
        }
    }
    pub fn generation(&self) -> &OperationRef {
        &self.generation
    }
    pub fn gate_root(&self) -> &OperationRef {
        &self.gate_root
    }
    pub fn operation(&self) -> &OperationRef {
        &self.operation
    }
    pub fn owner(&self) -> &Owner {
        &self.owner
    }
    pub fn generation_owner(&self) -> &Owner {
        &self.generation_owner
    }

    pub(super) fn validate(&self) -> Result<()> {
        for reference in [&self.generation, &self.gate_root, &self.operation] {
            validate_reference(reference)?;
        }
        validate_owner(&self.owner)?;
        validate_owner(&self.generation_owner)
    }
}

/// Registration and the exact work input commit together, under the parent's
/// context. A session child starts its own generation; a tool inherits its parent's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkRegistration {
    pub operation: OperationRef,
    pub kind: OperationKind,
    pub owner: Owner,
}

impl WorkRegistration {
    pub fn context(&self, parent: &ExecutionContext) -> ExecutionContext {
        let (generation, generation_owner) = match self.kind {
            OperationKind::Session => (self.operation.clone(), self.owner.clone()),
            OperationKind::Tool => (parent.generation.clone(), parent.generation_owner.clone()),
        };
        ExecutionContext::new(
            generation,
            parent.gate_root.clone(),
            self.operation.clone(),
            (self.owner.clone(), generation_owner),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    ToolReply,
    ModelResponse,
    SessionMetadata,
    Transcript,
    Progress,
    Lifecycle,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedOutput {
    /// Stable slot identity, e.g. a model round or progress sequence. A tool
    /// operation has only one ToolReply slot regardless of this ID.
    pub id: String,
    pub kind: OutputKind,
    /// Exact content, not permission for an unspecified later append.
    pub payload: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitAction {
    /// Stable per-operation idempotency identity. Changing content under this ID
    /// is an error, including after interruption or owner replacement.
    pub id: String,
    pub kind: GateAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GateAction {
    /// Admit an invocation attempt under an already registered operation.
    AdmitWork {
        input: Value,
    },
    /// Recovery bootstrap, common to saved-reply and replay branches. Original
    /// identity is retained; a replacement owner cannot adopt another generation.
    AdmitRecovery {
        original: ExecutionContext,
    },
    /// Recovery-only registration of work whose original parent already stopped.
    /// Creates no execution permission; retained lineage allows Cancel projection
    /// for a child whose worker never ran before the acceptance crash.
    RegisterStoppedWork {
        child: WorkRegistration,
    },
    StartWork {
        child: WorkRegistration,
        input: Value,
    },
    CommitOutput {
        output: CommittedOutput,
    },
    ConsumeReply {
        producer: ExecutionContext,
        reply: CommitReceipt,
    },
    /// Only physical progress of this exact owner. Cannot carry output or change
    /// logical state; old-generation cleanup remains possible after replacement.
    CleanupUpdate {
        cleanup: CleanupStatus,
    },
    /// Acknowledge conditional projection of this exact historical commit.
    /// External append/dedup and this cursor are NOT one transaction: adapters
    /// must recover a crash between them by checking the sink's durable commit ID.
    ProjectCommitted {
        commit: CommitReceipt,
        projector: String,
        expected_cursor: Option<CommitReceipt>,
    },
    /// Control-plane coverage of an already interrupted session. This contains
    /// no output. Only the current generation's owner can extend its Cancel
    /// projection to cover prompts admitted before the stop.
    RecordCancellation {
        through_seq: u64,
        /// Transcript audit fence includes lease renewals, unlike the stable owner identity.
        fence_token: u64,
    },
    FinishWork,
    /// Gate ownership handover. Lease acquisition/revocation adapters must use
    /// this boundary; an unrelated lease KV write cannot fence gate writers.
    ReplaceOwner {
        owner: Owner,
    },
    /// Root session generation replacement within the same stable gate anchor.
    /// The new root has no parent, so a stop on the old root cannot fence it.
    ReplaceGeneration {
        generation: OperationRef,
        owner: Owner,
    },
}

/// Historical proof, not a reusable permission to execute or append. Verify via
/// `ExecutionStore::committed_decision`; an arbitrary candidate ID is not proof.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitReceipt {
    pub gate_root: OperationRef,
    pub commit_id: String,
    pub sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptScope {
    pub gate_root: OperationRef,
    pub operation: OperationRef,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopReceipt {
    pub scope: OperationRef,
    pub decision: StopDecision,
    pub commit: CommitReceipt,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommittedAction {
    Open,
    Action {
        action: CommitAction,
    },
    Interrupt {
        scope: InterruptScope,
        decision: StopDecision,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedDecision {
    pub receipt: CommitReceipt,
    pub previous: Option<CommitReceipt>,
    pub expected_revision: u64,
    pub accepted_at: DateTime<Utc>,
    pub context: ExecutionContext,
    pub action: CommittedAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateCheckpoint {
    pub gate_root: OperationRef,
    pub epoch: String,
    pub through_sequence: u64,
}

pub(super) fn validate_reference(reference: &OperationRef) -> Result<()> {
    reference.validate()?;
    ensure!(
        reference.session_id.len() <= 256 && reference.execution_id.len() <= 256,
        "gate identity too long"
    );
    Ok(())
}

pub(super) fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= 256,
        "invalid gate action identity"
    );
    Ok(())
}

pub(super) fn validate_owner(owner: &Owner) -> Result<()> {
    validate_id(&owner.instance_id)?;
    ensure!(owner.fence > 0, "gate requires an owner fence");
    Ok(())
}

/// Terminal for this generation, never a model-recoverable tool failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interrupted {
    pub stop: StopReceipt,
}
impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "execution interrupted: {}",
            self.stop.decision.cancellation_id
        )
    }
}
impl std::error::Error for Interrupted {}
