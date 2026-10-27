//! Shared terminal settlement. HITL recovery can follow an existing continuation,
//! but never appends a missing prompt or changes its fixed ticket.
use super::authority::{OwnedTask, TaskReservation};
use super::*;
use crate::store::context::AdmissionPhase;

mod admission;
mod artifact;
mod hitl;
mod hitl_progress;
mod hitl_state;
mod outcome;

impl Runner {
    pub(super) async fn recover_admission(
        &self,
        session: &NatsSession,
        authority: &mut OwnedTask,
    ) -> Result<()> {
        admission::recover_admission(self, session, authority).await
    }

    async fn read_unowned_coordinated_task(
        &self,
        export: &Export,
        session: &NatsSession,
        task_id: &str,
    ) -> Result<TaskRecord> {
        let record = self
            .store
            .get_task(session.storage_key(), task_id)
            .await?
            .context("active task missing")?;
        Ok(self.live_record(export, record).await)
    }

    pub(super) async fn reconcile_coordinated(
        self: &Arc<Self>,
        export: &Export,
        session: &NatsSession,
        record: TaskRecord,
    ) -> Result<TaskRecord> {
        if record.task.status.state.is_terminal() {
            return Ok(record);
        }
        let Some(lease) = self.candidate_lease(session.storage_key()).await? else {
            return self
                .read_unowned_coordinated_task(export, session, &record.task.id)
                .await;
        };
        let authority = self.claim_context(session, lease).await?;
        // Keep the continuation launch future off blocking waiters' stack frames.
        Box::pin(self.recover_registered_task(export, session.clone(), authority)).await?;
        self.store
            .get_task(session.storage_key(), &record.task.id)
            .await?
            .context("recovered task missing")
    }
}

impl Runner {
    async fn ensure_first_reservation_allocated(
        &self,
        session: &NatsSession,
        authority: &mut OwnedTask,
        reservation: &crate::store::FirstMessageReservation,
    ) -> Result<()> {
        match &authority.context.document.state.active {
            None => {
                authority
                    .allocate(
                        &self.store,
                        session,
                        TaskReservation {
                            allocation: &reservation.allocation,
                            message: reservation.message.clone(),
                            predecessor: 0,
                        },
                    )
                    .await?;
                Ok(())
            }
            Some(active) => {
                anyhow::ensure!(
                    active.snapshot.task.id == reservation.allocation.task_id,
                    "reserved first task disappeared before recovery"
                );
                Ok(())
            }
        }
    }

    pub(crate) async fn recover_first_reservation(
        &self,
        session: &NatsSession,
        reservation: &crate::store::FirstMessageReservation,
        lease: Arc<harnx_runtime::nats_lease::NatsSessionLease>,
    ) -> Result<TaskRecord> {
        let mut authority = self.claim_context(session, lease).await?;
        self.ensure_first_reservation_allocated(session, &mut authority, reservation)
            .await?;
        self.recover_admission(session, &mut authority).await?;
        let record = self
            .store
            .get_task(session.storage_key(), &reservation.allocation.task_id)
            .await?
            .context("first reservation recovery missing task")?;
        authority
            .release(&self.store, session.storage_key())
            .await?;
        Ok(record)
    }
}

impl Runner {
    async fn fail_abandoned_legacy_orphan(
        &self,
        session: &NatsSession,
        task_id: &str,
        revision: u64,
    ) -> Result<TaskRecord> {
        #[cfg(feature = "fault-injection")]
        self.hooks
            .checkpoint(crate::fault_injection::Boundary::OrphanCancel)
            .await;
        if let Err(error) = session.cancel_pending_turn().await {
            warn!(%task_id, %error, "orphan remote cancellation failed");
        }
        self.store
            .update_task(
                TaskVersion {
                    storage_key: session.storage_key(),
                    task_id,
                    revision,
                },
                TaskChanges {
                    status: Some(status(
                        TaskState::Failed,
                        Some("interrupted by server restart"),
                    )),
                    ..Default::default()
                },
            )
            .await
    }

    pub(super) async fn reconcile_legacy_orphan(
        &self,
        access: TaskAccess<'_>,
        session: &NatsSession,
    ) -> Result<TaskRecord> {
        let TaskAccess {
            export,
            owner,
            task_id,
        } = access;
        let slot = self.slot(&ContextKey::new(export, session.session_id()));
        let mut active = slot.active.lock().await;
        if active
            .as_ref()
            .is_some_and(|handle| handle.done.has_changed().is_err() && !*handle.done.borrow())
        {
            // The supervisor vanished without settling. Fence its worker as an orphan.
            *active = None;
        }
        // A live turn may have completed while this caller waited for the gate.
        let record = self
            .store
            .get_task_for_export(export, owner, task_id)
            .await?
            .filter(|r| r.task.context_id == session.session_id())
            .ok_or(StoreError::NotFound)?;
        if record.task.status.state.is_terminal() {
            return Ok(record);
        }
        if let Some(result) = check_live_handle(&active, task_id, &record) {
            return result;
        }
        // A completed supervisor with nonterminal KV state failed persistence.
        // Treat it as abandoned, rather than waiting for a writer that has stopped.
        *active = None;
        self.fail_abandoned_legacy_orphan(session, task_id, record.revision)
            .await
    }
}

fn check_live_handle(
    active: &Option<Arc<RunnerHandle>>,
    task_id: &str,
    record: &TaskRecord,
) -> Option<Result<TaskRecord>> {
    let handle = active.as_ref().filter(|handle| !*handle.done.borrow())?;
    if handle.task_id == task_id {
        Some(Ok(record.clone()))
    } else {
        Some(Err(RunnerError::Busy.into()))
    }
}
