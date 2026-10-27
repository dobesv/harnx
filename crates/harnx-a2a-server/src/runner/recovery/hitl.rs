//! Resume only durable HITL continuations under their original fixed ticket.
use super::*;
use harnx_runtime::nats_session::fixed_admission::{FixedAdmissionOutcome, FixedAdmissionTicket};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RecoveryAction {
    Settle,
    Parked,
    Follow,
}

impl Runner {
    /// Reinspect released approvals on every sweep without churning owner CASes.
    pub(in crate::runner) async fn unchanged_parked(&self, session: &NatsSession) -> Result<bool> {
        hitl_state::check_unchanged_parked(self, session).await
    }

    async fn handle_recovery_progress_error(
        &self,
        session: &NatsSession,
        authority: &mut OwnedTask,
        error: anyhow::Error,
    ) -> Result<()> {
        if authority
            .context
            .document
            .state
            .active
            .as_ref()
            .is_some_and(|a| a.publication.pending.is_none())
        {
            authority
                .release(&self.store, session.storage_key())
                .await?;
        }
        #[cfg(feature = "fault-injection")]
        self.hooks
            .checkpoint(crate::fault_injection::Boundary::HitlRecoveryDeferred)
            .await;
        Err(error)
    }

    pub(in crate::runner) async fn recover_registered_task(
        self: &Arc<Self>,
        export: &Export,
        session: NatsSession,
        mut authority: OwnedTask,
    ) -> Result<()> {
        let action = match Box::pin(self.recover_hitl_progress(&session, &mut authority)).await {
            Ok(action) => action,
            Err(error) => {
                return self
                    .handle_recovery_progress_error(&session, &mut authority, error)
                    .await;
            }
        };
        match action {
            RecoveryAction::Follow => {
                Box::pin(self.resume_owned_hitl(export, session, authority)).await?;
                Ok(())
            }
            RecoveryAction::Settle => {
                Box::pin(self.recover_admission(&session, &mut authority)).await?;
                authority.release(&self.store, session.storage_key()).await
            }
            RecoveryAction::Parked => authority.release(&self.store, session.storage_key()).await,
        }
    }

    async fn check_fixed_admission_status(
        &self,
        session: &NatsSession,
        authority: &OwnedTask,
        ticket: &FixedAdmissionTicket,
    ) -> Result<bool> {
        if matches!(
            session.resolve_fixed_admission(ticket).await?,
            FixedAdmissionOutcome::Admitted { .. }
        ) {
            Ok(true)
        } else {
            hitl_progress::ensure_no_hitl_checkpoint(
                authority,
                "HITL checkpoint without admitted prompt",
            )?;
            Ok(false)
        }
    }

    async fn poll_recovered_progress(
        &self,
        session: &NatsSession,
        authority: &mut OwnedTask,
        ticket: &FixedAdmissionTicket,
    ) -> Result<RecoveryAction> {
        for _ in 0..3 {
            let progress = session.fixed_prompt_progress(ticket).await?;
            if let Some(action) =
                hitl_progress::classify_recovered_progress(self, session, authority, progress)
                    .await?
            {
                return Ok(action);
            }
        }
        anyhow::bail!("HITL progress did not stabilize after decision delivery")
    }

    async fn recover_hitl_progress(
        &self,
        session: &NatsSession,
        authority: &mut OwnedTask,
    ) -> Result<RecoveryAction> {
        authority
            .flush_pending(&self.store, session.storage_key())
            .await?;
        self.repair_hitl_mapping(session, authority).await?;
        if hitl_progress::should_settle_early(authority) {
            return Ok(RecoveryAction::Settle);
        }
        let ticket = authority.ticket(session.storage_key())?;
        if !self
            .check_fixed_admission_status(session, authority, &ticket)
            .await?
        {
            return Ok(RecoveryAction::Settle);
        }
        self.poll_recovered_progress(session, authority, &ticket)
            .await
    }
    pub(in crate::runner) async fn repair_hitl_mapping(
        &self,
        session: &NatsSession,
        authority: &OwnedTask,
    ) -> Result<()> {
        let active = authority
            .context
            .document
            .state
            .active
            .as_ref()
            .context("recovery task missing")?;
        if let Some(reply) = active
            .hitl_checkpoint
            .as_ref()
            .and_then(|c| c.reply.as_ref())
        {
            self.store
                .put_dedupe_mapping(
                    crate::store::DedupeMappingKind::Reply,
                    session.storage_key(),
                    crate::store::DedupeMappingTarget::new(
                        reply.message_identity(),
                        &active.snapshot.task.id,
                    ),
                )
                .await?;
        }
        Ok(())
    }
}
