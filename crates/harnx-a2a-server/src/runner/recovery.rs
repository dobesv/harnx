//! Shared foreground/background settlement. Never activates or appends a missing
//! prompt; only durable completion, closure or exact scoped stop permits release.
use super::authority::{stage_terminal, OwnedTask, TaskReservation};
use super::*;
use crate::store::context::AdmissionPhase;
mod artifact;
mod outcome;
use outcome::{recovery_outcome, recovery_status};

impl Runner {
    pub(super) async fn recover_admission(
        &self,
        session: &NatsSession,
        authority: &mut OwnedTask,
    ) -> Result<()> {
        authority
            .flush_pending(&self.store, session.storage_key())
            .await?;
        let active = authority
            .context
            .document
            .state
            .active
            .as_ref()
            .context("missing recovery task")?;
        if active.snapshot.task.status.state.is_terminal() {
            return authority.project_terminal(&self.store, session).await;
        }
        let ticket = authority.ticket(session.storage_key())?;
        let (state, text, phase, prompt) = recovery_outcome(
            session,
            &ticket,
            active.cancel.is_some(),
            super::limits::task_output_limit(&self.store, &authority.context).await?,
        )
        .await?;
        if let Some(text) = text.as_deref().filter(|_| state == TaskState::Completed) {
            artifact::recover_artifact(authority, &self.store, session, text).await?;
        }
        authority
            .update(&self.store, session.storage_key(), |active| {
                active.snapshot.task.status =
                    recovery_status(state.clone(), text.as_deref(), active.cancel.is_some());
                if state == TaskState::Completed {
                    active.snapshot.task.artifacts = text.clone().map(|text| vec![artifact(text)]);
                    if let Some(message) = active.snapshot.task.status.message.clone() {
                        active
                            .snapshot
                            .task
                            .history
                            .get_or_insert_with(Vec::new)
                            .push(message);
                    }
                }
                active.admission.phase = phase.clone();
                active.admission.prompt_sequence = prompt;
                if let Some(prompt) = prompt {
                    active.snapshot.user_msg_seq = prompt;
                }
                active.stop_confirmed = true;
                stage_terminal(active);
            })
            .await?;
        authority.project_terminal(&self.store, session).await
    }
    pub(super) async fn reconcile_coordinated(
        &self,
        export: &Export,
        session: &NatsSession,
        record: TaskRecord,
    ) -> Result<TaskRecord> {
        if record.task.status.state.is_terminal() {
            return Ok(record);
        }
        let Some(lease) = self.candidate_lease(session.storage_key()).await? else {
            // Absence of a local handle is never absence of an owner.
            let record = self
                .store
                .get_task(session.storage_key(), &record.task.id)
                .await?
                .context("active task missing")?;
            return Ok(self.live_record(export, record).await);
        };
        let mut authority = self.claim_context(session, lease).await?;
        self.recover_admission(session, &mut authority).await?;
        let record = self
            .store
            .get_task(session.storage_key(), &record.task.id)
            .await?
            .context("recovered task missing")?;
        authority
            .release(&self.store, session.storage_key())
            .await?;
        Ok(record)
    }
}

impl Runner {
    pub(crate) async fn recover_first_reservation(
        &self,
        session: &NatsSession,
        reservation: &crate::store::FirstMessageReservation,
        lease: Arc<harnx_runtime::nats_lease::NatsSessionLease>,
    ) -> Result<TaskRecord> {
        let mut authority = self.claim_context(session, lease).await?;
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
            }
            Some(active) => anyhow::ensure!(
                active.snapshot.task.id == reservation.allocation.task_id,
                "reserved first task disappeared before recovery"
            ),
        }
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
        if let Some(handle) = active.as_ref().filter(|handle| !*handle.done.borrow()) {
            if handle.task_id == task_id {
                return Ok(record);
            }
            // Never send a session-wide cancel at a newer live task.
            return Err(RunnerError::Busy.into());
        }
        // A completed supervisor with nonterminal KV state failed persistence.
        // Treat it as abandoned, rather than waiting for a writer that has stopped.
        *active = None;
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
                    revision: record.revision,
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
}
