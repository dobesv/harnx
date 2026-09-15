use super::{
    index::{self, Index, NodeRef},
    types::*,
};
use crate::{ExecutionStore, OperationRef};
use anyhow::{ensure, Context, Result};
use async_nats::jetstream::kv;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Head {
    pub root: OperationRef,
    pub epoch: String,
    pub state: Option<NodeRef>,
    pub tip: Option<CommitReceipt>,
    pub checkpoint: u64,
    /// Distinguish positive retry CAS attempts for lost-ack byte comparison.
    /// Without a nonce, another no-op CAS could falsely confirm our write.
    pub validation_nonce: Option<String>,
}

pub(super) struct Snapshot {
    pub head: Head,
    pub revision: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Proof {
    pub epoch: String,
    pub receipt: CommitReceipt,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Candidate {
    pub decision: CommittedDecision,
    pub state: Option<NodeRef>,
}

impl Snapshot {
    pub fn index<'a>(&self, store: &'a ExecutionStore) -> Index<'a> {
        Index {
            kv: &store.kv,
            prefix: prefix(&self.head.root),
            epoch: self.head.epoch.clone(),
            root: self.head.state.clone(),
        }
    }

    pub fn receipt(&self) -> Result<CommitReceipt> {
        let sequence = self
            .head
            .tip
            .as_ref()
            .map_or(0, |tip| tip.sequence)
            .checked_add(1)
            .context("gate sequence exhausted")?;
        Ok(CommitReceipt {
            gate_root: self.head.root.clone(),
            sequence,
            commit_id: uuid::Uuid::now_v7().to_string(),
        })
    }
}

impl ExecutionStore {
    pub(super) async fn gate_snapshot(&self, root: &OperationRef) -> Result<Snapshot> {
        self.optional_gate_snapshot(root)
            .await?
            .context("gate not initialized")
    }

    async fn optional_gate_snapshot(&self, root: &OperationRef) -> Result<Option<Snapshot>> {
        validate_reference(root)?;
        let Some(entry) =
            harnx_nats_common::recovery::read(|| self.kv.entry(anchor_key(root))).await?
        else {
            return Ok(None);
        };
        ensure!(entry.operation == kv::Operation::Put, "gate deleted");
        let head: Head = serde_json::from_slice(&entry.value)?;
        ensure!(head.root == *root, "gate root mismatch");
        Ok(Some(Snapshot {
            head,
            revision: entry.revision,
        }))
    }

    /// Historical high-water mark for recovery/projectors, not work permission.
    pub async fn gate_tip(&self, root: &OperationRef) -> Result<CommitReceipt> {
        self.gate_snapshot(root)
            .await?
            .head
            .tip
            .context("gate tip missing")
    }

    /// Return the exact committed action/payload, never an unattached candidate.
    /// This is historical proof, NOT current permission to consume or execute it.
    pub async fn committed_decision(&self, receipt: &CommitReceipt) -> Result<CommittedDecision> {
        let snapshot = self.gate_snapshot(&receipt.gate_root).await?;
        snapshot.index(self).decision(receipt).await
    }

    /// Resolve a durable sink message ID to its committed decision.
    pub async fn gate_decision_by_id(
        &self,
        root: &OperationRef,
        id: &str,
    ) -> Result<CommittedDecision> {
        self.gate_snapshot(root)
            .await?
            .index(self)
            .decision_by_id(id)
            .await
    }

    pub async fn gate_generation(
        &self,
        root: &OperationRef,
        session: &str,
    ) -> Result<OperationRef> {
        self.gate_snapshot(root)
            .await?
            .index(self)
            .get(&super::state::generation_key(session))
            .await?
            .context("gate generation missing")
    }

    /// A binding can be visible before the first StartWork CAS. Live followers
    /// deny output in that window rather than treating preparation as failure.
    pub async fn gate_generation_if_registered(
        &self,
        root: &OperationRef,
        session: &str,
    ) -> Result<Option<OperationRef>> {
        let Some(snapshot) = self.optional_gate_snapshot(root).await? else {
            return Ok(None);
        };
        snapshot
            .index(self)
            .get(&super::state::generation_key(session))
            .await
    }

    /// Reconcile a lost action acknowledgement, including after its scope stopped.
    /// Unlike `commit_if_admissible`, this returns historical evidence only and
    /// never authorizes new work or consumption. Identity must match exactly.
    pub async fn committed_action(
        &self,
        ctx: &ExecutionContext,
        action: &CommitAction,
    ) -> Result<Option<CommittedDecision>> {
        ctx.validate()?;
        validate_id(&action.id)?;
        let snapshot = self.gate_snapshot(&ctx.gate_root).await?;
        let index = snapshot.index(self);
        let Some(receipt) = index
            .get::<CommitReceipt>(&super::state::action_key(ctx, &action.id))
            .await?
        else {
            return Ok(None);
        };
        let decision = index.decision(&receipt).await?;
        ensure!(
            decision.context == *ctx
                && decision.action
                    == CommittedAction::Action {
                        action: action.clone()
                    },
            "action identity reused with different content"
        );
        Ok(Some(decision))
    }

    /// Read a committed sequence for a projector/recovery scan. A missing sequence
    /// is an error, not a retention gap to skip. Projectors keep a durable cursor.
    pub async fn gate_decision_at(
        &self,
        root: &OperationRef,
        sequence: u64,
    ) -> Result<CommittedDecision> {
        let snapshot = self.gate_snapshot(root).await?;
        let index = snapshot.index(self);
        let receipt: CommitReceipt = index
            .get(&sequence_key(sequence))
            .await?
            .context("gate sequence missing")?;
        index.decision(&receipt).await
    }

    /// Historical tool-reply slot, including a commit whose journal projection
    /// was lost. The caller still needs fresh consumption admission.
    pub async fn committed_tool_reply(
        &self,
        ctx: &ExecutionContext,
    ) -> Result<Option<CommittedDecision>> {
        let snapshot = self.gate_snapshot(ctx.gate_root()).await?;
        let index = snapshot.index(self);
        let key = super::actions::output_key(
            ctx,
            &CommittedOutput {
                id: "reply".into(),
                kind: OutputKind::ToolReply,
                payload: serde_json::Value::Null,
            },
        )?;
        match index.get::<CommitReceipt>(&key).await? {
            Some(receipt) => Ok(Some(index.decision(&receipt).await?)),
            None => Ok(None),
        }
    }

    /// Query retained gate lineage without consulting physical graph records.
    /// Negative results are snapshots, not permission to commit output.
    pub async fn gate_stop(
        &self,
        root: &OperationRef,
        operation: &OperationRef,
    ) -> Result<Option<StopReceipt>> {
        self.gate_snapshot(root)
            .await?
            .index(self)
            .stop_for(operation)
            .await
    }

    /// Follower-only stop observation during registration. Absence means keep
    /// waiting, never permission to execute. Both lookups use the same snapshot.
    pub(super) async fn gate_stop_if_registered(
        &self,
        root: &OperationRef,
        operation: &OperationRef,
    ) -> Result<Option<StopReceipt>> {
        let Some(snapshot) = self.optional_gate_snapshot(root).await? else {
            return Ok(None);
        };
        let index = snapshot.index(self);
        if index
            .get::<super::state::Member>(&super::state::member_key(operation))
            .await?
            .is_none()
        {
            return Ok(None);
        }
        index.stop_for(operation).await
    }

    pub(super) async fn persist_candidate(
        &self,
        snapshot: &Snapshot,
        mut index: Index<'_>,
        decision: CommittedDecision,
    ) -> Result<Head> {
        let proof = Proof {
            epoch: snapshot.head.epoch.clone(),
            receipt: decision.receipt.clone(),
        };
        index
            .set(&proof_key(&decision.receipt.commit_id), &proof)
            .await?;
        index
            .set(&sequence_key(decision.receipt.sequence), &decision.receipt)
            .await?;
        let mut head = snapshot.head.clone();
        head.state = index.root.clone();
        head.tip = Some(decision.receipt.clone());
        head.validation_nonce = None;
        let key = decision_key(&proof)?;
        index::create(
            &self.kv,
            &key,
            &Candidate {
                decision,
                state: index.root,
            },
        )
        .await?;
        Ok(head)
    }

    pub(super) async fn cas_gate(&self, revision: u64, head: &Head) -> Result<bool> {
        match harnx_nats_common::cas::update(
            &self.kv,
            anchor_key(&head.root),
            serde_json::to_vec(head)?.into(),
            revision,
        )
        .await
        {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == kv::UpdateErrorKind::WrongLastRevision => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

impl Index<'_> {
    async fn decision_by_id(&self, id: &str) -> Result<CommittedDecision> {
        index::validate_token(id)?;
        let proof: Proof = self
            .get(&proof_key(id))
            .await?
            .context("candidate is not committed")?;
        self.decision(&proof.receipt).await
    }

    pub async fn decision(&self, receipt: &CommitReceipt) -> Result<CommittedDecision> {
        validate_reference(&receipt.gate_root)?;
        index::validate_token(&receipt.commit_id)?;
        ensure!(
            prefix(&receipt.gate_root) == self.prefix,
            "commit belongs to another gate"
        );
        let proof: Proof = self
            .get(&proof_key(&receipt.commit_id))
            .await?
            .context("candidate is not committed")?;
        ensure!(proof.receipt == *receipt, "commit proof mismatch");
        let candidate: Candidate = index::read(self.kv, &decision_key(&proof)?).await?;
        ensure!(
            candidate.decision.receipt == *receipt,
            "committed decision mismatch"
        );
        Ok(candidate.decision)
    }
}

pub(super) fn prefix(root: &OperationRef) -> String {
    format!("sessions/{}/gates/{}/", root.session_id, root.execution_id)
}
pub(super) fn anchor_key(root: &OperationRef) -> String {
    format!("{}head", prefix(root))
}
pub(super) fn proof_key(id: &str) -> String {
    format!("proof/{id}")
}
pub(super) fn sequence_key(sequence: u64) -> String {
    format!("sequence/{sequence}")
}
pub(super) fn decision_key(proof: &Proof) -> Result<String> {
    validate_reference(&proof.receipt.gate_root)?;
    index::validate_token(&proof.epoch)?;
    index::validate_token(&proof.receipt.commit_id)?;
    Ok(format!(
        "{}decisions/{}/{}",
        prefix(&proof.receipt.gate_root),
        proof.epoch,
        proof.receipt.commit_id
    ))
}
