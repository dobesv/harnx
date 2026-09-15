//! Physical-node retirement keeps stop evidence on the original CAS key.

use super::*;
use serde::{Deserialize, Serialize};

// Active records retain their wire shape for existing readers. The retired tag
// cannot deserialize as OperationState, and malformed active records must not
// fall back to an apparently unfenced lineage record.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum StoredOperation {
    Active(Box<Operation>),
    Retired(Box<RetiredOperation>),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetiredOperation {
    state: RetiredState,
    reference: OperationRef,
    parent: Option<OperationRef>,
    #[serde(default)]
    previous_generation: Option<OperationRef>,
    stop_decision: Option<StopDecision>,
    #[serde(default)]
    kind: Option<OperationKind>,
    #[serde(default)]
    admissions: BTreeMap<String, Option<u64>>,
    #[serde(default)]
    owner_fences: BTreeSet<u64>,
    #[serde(default)]
    gate_registration: Option<Box<crate::GateRegistration>>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RetiredState {
    Retired,
}

impl From<Operation> for RetiredOperation {
    fn from(operation: Operation) -> Self {
        Self {
            state: RetiredState::Retired,
            reference: operation.reference,
            parent: operation.parent,
            previous_generation: operation.previous_generation,
            stop_decision: operation.stop_decision,
            kind: Some(operation.kind),
            admissions: operation.admissions,
            owner_fences: (*operation.owner_fences)
                .into_iter()
                .chain(operation.owner.map(|owner| owner.fence))
                .collect(),
            gate_registration: operation.gate_registration,
        }
    }
}

impl StoredOperation {
    fn into_operation(self) -> Option<Operation> {
        match self {
            Self::Active(operation) => Some(*operation),
            Self::Retired(_) => None,
        }
    }

    fn lineage(self) -> RetiredOperation {
        match self {
            Self::Active(operation) => (*operation).into(),
            Self::Retired(operation) => *operation,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RecoveryHistory {
    pub reference: OperationRef,
    pub kind: Option<OperationKind>,
    pub admissions: BTreeMap<String, Option<u64>>,
    pub owner_fences: BTreeSet<u64>,
    pub gate_registration: Option<Box<crate::GateRegistration>>,
}

impl ExecutionStore {
    /// Original prompt and worker-fence evidence survives physical retirement.
    /// This is identity evidence only; recovery must still pass the tree gate.
    pub async fn recovery_history(&self, session: &str) -> Result<Vec<RecoveryHistory>> {
        OperationRef::new(session, "history").validate()?;
        // A LastPerSubject key consumer can finish early while this busy bucket
        // is being updated. Follow the retained winning-generation chain instead;
        // a partial enumeration must never make an admitted prompt look unbound.
        let mut next = self
            .current(session)
            .await?
            .map(|operation| operation.reference);
        let mut seen = BTreeSet::new();
        let mut history = Vec::new();
        while let Some(reference) = next {
            ensure!(seen.insert(reference.clone()), "recovery generation cycle");
            let (record, _) = self
                .record_entry(&reference.key())
                .await?
                .context("recovery history missing")?;
            let record = record.lineage();
            ensure!(
                record.reference == reference && reference.session_id == session,
                "recovery generation identity mismatch"
            );
            next = record.previous_generation;
            history.push(RecoveryHistory {
                reference: record.reference,
                kind: record.kind,
                admissions: record.admissions,
                owner_fences: record.owner_fences,
                gate_registration: record.gate_registration,
            });
        }
        Ok(history)
    }

    pub(crate) async fn recovery_registration(
        &self,
        reference: &OperationRef,
    ) -> Result<Option<Box<crate::GateRegistration>>> {
        let (record, _) = self
            .record_entry(&reference.key())
            .await?
            .context("recovery lineage missing")?;
        Ok(record.lineage().gate_registration)
    }

    /// Read a physical graph node. Retired nodes return `None`; use
    /// `stop_decision` to resolve their retained stop evidence and lineage.
    pub async fn get(&self, reference: &OperationRef) -> Result<Option<Operation>> {
        reference.validate()?;
        Ok(self
            .entry(&reference.key())
            .await?
            .map(|(operation, _)| operation))
    }

    pub(super) async fn entry(&self, key: &str) -> Result<Option<(Operation, u64)>> {
        Ok(self
            .record_entry(key)
            .await?
            .and_then(|(record, revision)| {
                record
                    .into_operation()
                    .map(|operation| (operation, revision))
            }))
    }

    /// Resolve the nearest accepted stop for this exact generation or its
    /// ancestors, including retired physical nodes. Unknown identity/lineage is
    /// an error. `None` is only a snapshot, NEVER permission to commit output.
    pub async fn stop_decision(&self, reference: &OperationRef) -> Result<Option<StopDecision>> {
        let mut next = Some(reference.clone());
        let mut seen = BTreeSet::new();
        while let Some(reference) = next {
            reference.validate()?;
            ensure!(
                seen.insert(reference.clone()),
                "execution stop lineage cycle"
            );
            let (record, _) = self
                .record_entry(&reference.key())
                .await?
                .context("execution stop lineage missing")?;
            let record = record.lineage();
            ensure!(
                record.reference == reference,
                "execution stop identity mismatch"
            );
            if let Some(stop) = record.stop_decision {
                return Ok(Some(stop));
            }
            next = record.parent;
        }
        Ok(None)
    }

    /// Fail closed on unknown generations. Positive reads are durable stop
    /// evidence; negative reads do not close cancel-versus-output races (#1878).
    pub async fn is_stop_fenced(&self, reference: &OperationRef) -> Result<bool> {
        Ok(self.stop_decision(reference).await?.is_some())
    }

    async fn record_entry(&self, key: &str) -> Result<Option<(StoredOperation, u64)>> {
        let Some(entry) = harnx_nats_common::recovery::read(|| self.kv.entry(key)).await? else {
            return Ok(None);
        };
        if entry.operation != kv::Operation::Put {
            return Ok(None);
        }
        Ok(Some((
            serde_json::from_slice(&entry.value)?,
            entry.revision,
        )))
    }

    pub(super) async fn retire_observed(&self, reference: &OperationRef) -> Result<()> {
        loop {
            let Some((operation, revision)) = self.entry(&reference.key()).await? else {
                return Ok(());
            };
            if !operation.can_prune() || self.retained_by_graph(&operation).await? {
                return Ok(());
            }
            // CAS replaces, never deletes, the evidence. A concurrent cleanup or
            // interruption update must be reread rather than purged from a stale
            // snapshot. Retired records cannot be claimed or recreated.
            let retired = RetiredOperation::from(operation);
            match harnx_nats_common::cas::update(
                &self.kv,
                reference.key(),
                serde_json::to_vec(&retired)?.into(),
                revision,
            )
            .await
            {
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == kv::UpdateErrorKind::WrongLastRevision => {
                    tokio::task::yield_now().await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    async fn retained_by_graph(&self, operation: &Operation) -> Result<bool> {
        if operation.kind == OperationKind::Session
            && self
                .current(&operation.reference.session_id)
                .await?
                .is_some_and(|current| current.reference == operation.reference)
        {
            return Ok(true);
        }
        if let Some(parent) = &operation.parent {
            return Ok(self
                .get(parent)
                .await?
                .is_some_and(|parent| parent.children.contains(&operation.reference)));
        }
        Ok(false)
    }
}

#[cfg(test)]
#[path = "retention_tests.rs"]
mod tests;
