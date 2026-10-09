//! CAS authority used by coordinated admission and task mutations.
//! Lease selection is prospective. Only this document's CAS establishes an
//! owner epoch. Never rebase a prepared write after conflict or uncertain ack.
use super::A2aStore;
use anyhow::{ensure, Context, Result};
use async_nats::jetstream::kv;
use harnx_nats_common::leader_reads;
use harnx_runtime::nats_lease::NatsSessionLease;

mod cancellation;
mod projections;
mod schema;
pub(crate) mod terminal_event;
mod writes;
pub use schema::*;

pub fn context_authority_key(storage_key: &str) -> String {
    format!("sessions/{storage_key}/a2a/context")
}

/// Binding resolved by the caller before trusted coordination operations.
pub struct ContextIdentity<'a> {
    pub storage_key: &'a str,
    pub local_id: &'a str,
}

#[derive(Debug, Clone)]
pub struct ContextSnapshot {
    pub document: ContextDocument,
    /// Bucket revision. Task-local revisions are not CAS preconditions here.
    pub revision: u64,
}
impl ContextSnapshot {
    pub fn version(&self) -> Result<ContextVersion> {
        Ok(ContextVersion {
            owner: self
                .document
                .owner
                .clone()
                .context("context has no owner")?,
            task_id: self
                .document
                .state
                .active
                .as_ref()
                .map(|active| active.snapshot.task.id.clone()),
            revision: self.revision,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ContextVersion {
    pub owner: OwnerFence,
    pub task_id: Option<String>,
    pub revision: u64,
}

/// Immutable operation identity and predecessor. Retain this exact ticket until
/// its acknowledgement is resolved; don't prepare a replacement on uncertainty.
#[derive(Debug, Clone)]
pub struct ContextWrite {
    storage_key: String,
    predecessor: u64,
    document: ContextDocument,
}
impl ContextWrite {
    pub fn document(&self) -> &ContextDocument {
        &self.document
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorityError {
    Conflict,
    StaleOwner,
    TaskMismatch,
    OperationMismatch,
    LegacyWrite,
    Deleted,
}
impl std::fmt::Display for AuthorityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "A2A authority {:?}", self)
    }
}
impl std::error::Error for AuthorityError {}

impl A2aStore {
    /// Trusted coordination read. RPC callers must resolve the context binding first.
    /// STREAM.MSG.GET, never follower/direct KV reads or process-local handles.
    pub async fn read_context(&self, storage_key: &str) -> Result<Option<ContextSnapshot>> {
        let key = context_authority_key(storage_key);
        let entry = leader_reads::entry(self.store.kv_store(), &key).await?;
        let Some(entry) = entry else {
            return Ok(None);
        };
        ensure!(
            entry.operation == kv::Operation::Put,
            AuthorityError::Deleted
        );
        let document: ContextDocument = serde_json::from_slice(&entry.value)?;
        document.validate()?;
        Ok(Some(ContextSnapshot {
            document,
            revision: entry.revision,
        }))
    }

    pub async fn prepare_context_claim(
        &self,
        identity: ContextIdentity<'_>,
        lease: &NatsSessionLease,
        operation_id: &str,
    ) -> Result<ContextWrite> {
        let ContextIdentity {
            storage_key,
            local_id,
        } = identity;
        ensure!(
            lease.key() == format!("sessions/{storage_key}/a2a/lock")
                && lease.bucket_name() == "harnx_leases",
            "claim requires this context's scoped A2A lease"
        );
        ensure!(
            !lease.worker_id().is_empty() && lease.revalidate_ownership().await?,
            "prospective lease lost"
        );
        let current = self.read_context(storage_key).await?;
        let predecessor = current.as_ref().map_or(0, |current| current.revision);
        let mut document = match current {
            Some(current) => current.document,
            None => ContextDocument {
                version: 1,
                local_id: local_id.into(),
                epoch: 0,
                owner: None,
                last_lease_revision: 0,
                state: ContextState::default(),
                last_operation: OperationReceipt {
                    id: String::new(),
                    digest: String::new(),
                    predecessor: 0,
                },
            },
        };
        ensure!(document.local_id == local_id, "context local id mismatch");
        ensure!(
            lease.acquisition_revision() > document.last_lease_revision,
            AuthorityError::StaleOwner
        );
        document.epoch = document
            .epoch
            .checked_add(1)
            .context("owner epoch overflow")?;
        document.owner = Some(OwnerFence {
            boot_id: lease.worker_id().into(),
            epoch: document.epoch,
        });
        document.last_lease_revision = lease.acquisition_revision();
        ContextWrite::new(storage_key, predecessor, document, operation_id)
    }

    pub(super) async fn owner_document(
        &self,
        storage_key: &str,
        expected: &ContextVersion,
    ) -> Result<ContextSnapshot> {
        let current = self
            .read_context(storage_key)
            .await?
            .context("context authority missing")?;
        ensure!(
            current.document.owner.as_ref() == Some(&expected.owner),
            AuthorityError::StaleOwner
        );
        let task_id = current
            .document
            .state
            .active
            .as_ref()
            .map(|active| &active.snapshot.task.id);
        ensure!(
            task_id == expected.task_id.as_ref(),
            AuthorityError::TaskMismatch
        );
        ensure!(
            current.revision == expected.revision,
            AuthorityError::Conflict
        );
        Ok(current)
    }

    pub(super) async fn reject_legacy_write(&self, storage_key: &str) -> Result<()> {
        ensure!(
            self.read_context(storage_key).await?.is_none(),
            AuthorityError::LegacyWrite
        );
        Ok(())
    }
}
