//! Every activated task mutation CASes its owning context document.
use super::*;
use crate::store::{
    context::{ActiveTask, ContextSnapshot},
    MessageIdentity, TaskAllocation,
};
use harnx_runtime::{
    nats_lease::NatsSessionLease, nats_session::fixed_admission::FixedAdmissionTicket,
};

mod allocation;

pub(super) struct TaskReservation<'a> {
    pub allocation: &'a TaskAllocation,
    pub message: Message,
    pub predecessor: u64,
}

pub(super) struct OwnedTask {
    pub lease: Arc<NatsSessionLease>,
    pub context: ContextSnapshot,
}
impl OwnedTask {
    pub fn ticket(&self, storage: &str) -> Result<FixedAdmissionTicket> {
        Self::ticket_from_context(&self.context, storage)
    }

    pub fn ticket_from_context(
        context: &ContextSnapshot,
        storage: &str,
    ) -> Result<FixedAdmissionTicket> {
        let admission = &context
            .document
            .state
            .active
            .as_ref()
            .context("active task missing")?
            .admission;
        FixedAdmissionTicket::from_parts(
            storage.into(),
            admission.invocation_id.clone(),
            admission.prompt_id.clone(),
            admission
                .closure_id
                .clone()
                .context("admission closure identity missing")?,
            admission.fixed_predecessor,
        )
    }
    async fn read_and_validate_version(
        &self,
        store: &A2aStore,
        storage: &str,
        expected: &crate::store::context::ContextVersion,
    ) -> Result<crate::store::context::ContextVersion> {
        anyhow::ensure!(
            self.lease.revalidate_ownership().await?,
            "A2A scoped lease lost"
        );
        let current = store
            .read_context(storage)
            .await?
            .context("owned context missing")?;
        let version = current.version()?;
        anyhow::ensure!(
            version.owner == expected.owner && version.task_id == expected.task_id,
            crate::store::context::AuthorityError::StaleOwner
        );
        Ok(version)
    }

    async fn commit_update_write(
        &mut self,
        store: &A2aStore,
        write: &crate::store::context::ContextWrite,
    ) -> Result<Option<TaskRecord>> {
        match store.commit_context(write).await {
            Ok(context) => {
                self.context = context;
                let active = self
                    .context
                    .document
                    .state
                    .active
                    .as_ref()
                    .context("active task missing after CAS")?;
                Ok(Some(active.snapshot.clone()))
            }
            Err(e) if is_authority_conflict(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn try_update_attempt(
        &mut self,
        store: &A2aStore,
        storage: &str,
        mutate: &mut impl FnMut(&mut ActiveTask),
    ) -> Result<Option<TaskRecord>> {
        let expected = self.context.version()?;
        let version = self
            .read_and_validate_version(store, storage, &expected)
            .await?;
        let op_id = uuid::Uuid::new_v4().to_string();
        let prepared = store
            .prepare_context_update(storage, &version, &op_id, |state| {
                mutate(state.active.as_mut().expect("owned task"))
            })
            .await;
        let write = match prepared {
            Ok(write) => write,
            Err(e) if is_authority_conflict(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        self.commit_update_write(store, &write).await
    }

    pub async fn update(
        &mut self,
        store: &A2aStore,
        storage: &str,
        mut mutate: impl FnMut(&mut ActiveTask),
    ) -> Result<TaskRecord> {
        for _ in 0..8 {
            if let Some(record) = self.try_update_attempt(store, storage, &mut mutate).await? {
                return Ok(record);
            }
        }
        anyhow::bail!(crate::store::context::AuthorityError::Conflict)
    }
    pub async fn allocate(
        &mut self,
        store: &A2aStore,
        session: &NatsSession,
        reservation: TaskReservation<'_>,
    ) -> Result<TaskRecord> {
        let active = reservation.into_active();
        let write = store
            .prepare_context_update(
                session.storage_key(),
                &self.context.version()?,
                &uuid::Uuid::new_v4().to_string(),
                |state| state.active = Some(active),
            )
            .await?;
        self.context = store.commit_context(&write).await?;
        let record = self
            .context
            .document
            .state
            .active
            .as_ref()
            .context("allocated task missing")?
            .snapshot
            .clone();
        store.repair_index(session.storage_key(), &record).await?;
        Ok(record)
    }
    pub async fn project_terminal(
        &mut self,
        store: &A2aStore,
        session: &NatsSession,
    ) -> Result<()> {
        let active = self
            .context
            .document
            .state
            .active
            .as_ref()
            .context("terminal task missing")?
            .clone();
        anyhow::ensure!(
            active.snapshot.task.status.state.is_terminal() && active.stop_confirmed,
            "task has no durable stop proof"
        );
        store
            .archive_context_terminal(session.storage_key(), &active.snapshot.task.id)
            .await?;
        store
            .put_dedupe_mapping(
                crate::store::DedupeMappingKind::Message,
                session.storage_key(),
                crate::store::DedupeMappingTarget::new(
                    MessageIdentity {
                        message_id: &active.message.message_id,
                        fingerprint: &active.message.fingerprint,
                    },
                    &active.snapshot.task.id,
                ),
            )
            .await?;
        self.flush_pending(store, session.storage_key()).await?;
        self.update(store, session.storage_key(), |active| {
            active.projections.archive = true;
            active.projections.message_mapping = true;
        })
        .await?;
        store
            .repair_index(session.storage_key(), &active.snapshot)
            .await?;
        Ok(())
    }
    pub async fn release(&self, store: &A2aStore, storage: &str) -> Result<()> {
        let write = store
            .prepare_context_release(
                storage,
                &self.context.version()?,
                &uuid::Uuid::new_v4().to_string(),
            )
            .await?;
        store.commit_context(&write).await?;
        self.lease.release().await
    }
}

pub(super) fn stage_terminal(active: &mut ActiveTask) {
    let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: active.snapshot.task.id.clone(),
        context_id: active.snapshot.task.context_id.clone(),
        status: active.snapshot.task.status.clone(),
        metadata: None,
    });
    let id = format!("{}:terminal", active.snapshot.task.id);
    super::outbox::stage_event(active, event, &id);
}

fn is_authority_conflict(error: &anyhow::Error) -> bool {
    error.downcast_ref::<crate::store::context::AuthorityError>()
        == Some(&crate::store::context::AuthorityError::Conflict)
}
