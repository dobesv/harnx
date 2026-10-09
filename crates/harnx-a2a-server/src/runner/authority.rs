//! Every activated task mutation CASes its owning context document.
use super::*;
use crate::store::{
    context::{ActiveTask, AdmissionPhase, AdmissionState, ContextSnapshot, RetainedMessage},
    MessageIdentity, TaskAllocation,
};
use harnx_runtime::{
    nats_lease::NatsSessionLease, nats_session::fixed_admission::FixedAdmissionTicket,
};

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
        let admission = &self
            .context
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
    pub async fn update(
        &mut self,
        store: &A2aStore,
        storage: &str,
        mut mutate: impl FnMut(&mut ActiveTask),
    ) -> Result<TaskRecord> {
        let expected = self.context.version()?;
        for _ in 0..8 {
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
            // Only pure document mutation retries. Runtime effects keep their
            // original ticket; a newer receipt never authorizes replay.
            let prepared = store
                .prepare_context_update(
                    storage,
                    &version,
                    &uuid::Uuid::new_v4().to_string(),
                    |state| mutate(state.active.as_mut().expect("owned task")),
                )
                .await;
            let write = match prepared {
                Ok(write) => write,
                Err(error)
                    if error.downcast_ref::<crate::store::context::AuthorityError>()
                        == Some(&crate::store::context::AuthorityError::Conflict) =>
                {
                    continue
                }
                Err(error) => return Err(error),
            };
            match store.commit_context(&write).await {
                Ok(context) => {
                    self.context = context;
                    return Ok(self
                        .context
                        .document
                        .state
                        .active
                        .as_ref()
                        .context("active task missing after CAS")?
                        .snapshot
                        .clone());
                }
                Err(error)
                    if error.downcast_ref::<crate::store::context::AuthorityError>()
                        == Some(&crate::store::context::AuthorityError::Conflict) =>
                {
                    continue
                }
                Err(error) => return Err(error),
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
        let TaskReservation {
            allocation,
            message,
            predecessor,
        } = reservation;
        let fingerprint = crate::store::message_fingerprint(&message.parts);
        let active = ActiveTask {
            snapshot: TaskRecord {
                version: 1,
                task: Task {
                    id: allocation.task_id.clone(),
                    context_id: allocation.local_id.clone(),
                    status: status(TaskState::Submitted, None),
                    artifacts: None,
                    history: Some(vec![message.clone()]),
                    metadata: None,
                },
                user_msg_id: allocation.prompt_id.clone(),
                user_msg_seq: 0,
                execution_id: allocation.invocation_id.clone(),
                revision: 1,
                stream_seq: 0,
                created_at: allocation.created_at,
                updated_at: allocation.created_at,
            },
            message: RetainedMessage {
                message_id: message.message_id,
                fingerprint,
            },
            admission: AdmissionState {
                invocation_id: allocation.invocation_id.clone(),
                prompt_id: allocation.prompt_id.clone(),
                closure_id: Some(allocation.closure_id.clone()),
                fixed_predecessor: predecessor,
                phase: AdmissionPhase::Reserved,
                prompt_sequence: None,
            },
            cancel: None,
            publication: Default::default(),
            projections: Default::default(),
            stop_confirmed: false,
        };
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
            .put_message_dedupe(
                session.storage_key(),
                MessageIdentity {
                    message_id: &active.message.message_id,
                    fingerprint: &active.message.fingerprint,
                },
                &active.snapshot.task.id,
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
