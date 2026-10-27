//! Registry entries are hints. Context CAS and fixed runtime identity decide work.
use super::supervision::SupervisionConfig;
use super::*;
use crate::store::{
    context::ContextSnapshot, registry::RecoveryRegistration, FirstMessageReservation,
};
use harnx_runtime::nats_lease::NatsSessionLease;

struct RegisteredRecovery {
    target: RecoveryTarget,
    lease: Option<Arc<NatsSessionLease>>,
}

enum RecoveryTarget {
    Active,
    First(Box<FirstMessageReservation>),
}

impl Runner {
    async fn check_registration_eligible(
        &self,
        export: &Export,
        entry: &RecoveryRegistration,
    ) -> Result<Option<RecoveryTarget>> {
        entry.validate_export(export)?;
        if self.store.prune_unused_registration(entry).await? {
            return Ok(None);
        }
        self.recovery_target(export, entry).await
    }

    async fn prepare_registered_recovery(
        &self,
        export: &Export,
        entry: &RecoveryRegistration,
    ) -> Result<Option<RegisteredRecovery>> {
        let Some(target) = self.check_registration_eligible(export, entry).await? else {
            return Ok(None);
        };
        // First-message initialization needs its lease before any session create.
        // Active sessions can be inspected without claiming a parked approval.
        let lease = if matches!(&target, RecoveryTarget::First(_)) {
            let Some(lease) = self.candidate_lease(&entry.allocation.storage_key).await? else {
                return Ok(None);
            };
            Some(lease)
        } else {
            None
        };
        Ok(Some(RegisteredRecovery { target, lease }))
    }

    async fn recover_registered_first(
        &self,
        session: &NatsSession,
        first: &FirstMessageReservation,
        lease: Option<Arc<NatsSessionLease>>,
    ) -> Result<()> {
        self.recover_first_reservation(
            session,
            first,
            lease.context("first recovery lease missing")?,
        )
        .await?;
        Ok(())
    }

    async fn recover_registered_active(
        self: &Arc<Self>,
        export: &Export,
        session: NatsSession,
        storage: &str,
    ) -> Result<()> {
        if self.unchanged_parked(&session).await? {
            #[cfg(feature = "fault-injection")]
            self.hooks
                .checkpoint(crate::fault_injection::Boundary::HitlParkedInspected)
                .await;
            return Ok(());
        }
        let Some(lease) = self.candidate_lease(storage).await? else {
            return Ok(());
        };
        let authority = self.claim_context(&session, lease).await?;
        if authority.context.document.state.active.is_some() {
            self.recover_registered_task(export, session, authority)
                .await
        } else {
            authority.release(&self.store, storage).await
        }
    }

    pub(super) async fn reconcile_registration(
        self: &Arc<Self>,
        export: &Export,
        entry: &RecoveryRegistration,
        settings: &SupervisionConfig,
    ) -> Result<()> {
        let Some(recovery) = self.prepare_registered_recovery(export, entry).await? else {
            return Ok(());
        };
        let RegisteredRecovery { target, lease } = recovery;
        let is_first = matches!(&target, RecoveryTarget::First(_));
        let owner = entry
            .owner
            .clone()
            .map(Principal::User)
            .unwrap_or(Principal::Anonymous);
        let session = self
            .stored_session(
                super::stored_session::StoredSessionRequest {
                    export,
                    owner: &owner,
                    global_config: &settings.config,
                    activation_route: settings.route.clone(),
                    abort: settings.abort.clone(),
                    allow_create: is_first,
                },
                &entry.allocation,
            )
            .await?;
        let storage = &entry.allocation.storage_key;
        match target {
            RecoveryTarget::First(first) => {
                self.recover_registered_first(&session, &first, lease).await
            }
            RecoveryTarget::Active => {
                self.recover_registered_active(export, session, storage)
                    .await
            }
        }
    }

    async fn check_context_recovery_target(
        &self,
        export: &Export,
        storage_key: &str,
    ) -> Result<Option<Option<RecoveryTarget>>> {
        let Some(context) = self.store.read_context(storage_key).await? else {
            return Ok(None);
        };
        crate::diagnostics::pending(&context);
        self.store.cleanup_context_events(storage_key).await?;
        let Some(active) = &context.document.state.active else {
            return Ok(None);
        };
        if active.ready_to_retire() || self.service_local_owner(export, &context).await? {
            return Ok(Some(None));
        }
        Ok(Some(Some(RecoveryTarget::Active)))
    }

    async fn check_first_recovery_target(
        &self,
        entry: &RecoveryRegistration,
    ) -> Result<Option<RecoveryTarget>> {
        let first = self.registered_first(entry).await?;
        if self
            .store
            .get_task(&entry.allocation.storage_key, &entry.allocation.task_id)
            .await?
            .is_some()
        {
            return Ok(None);
        }
        if let Some(first) = &first {
            crate::diagnostics::age("admission", first.allocation.created_at);
        }
        Ok(first.map(|first| RecoveryTarget::First(Box::new(first))))
    }

    async fn recovery_target(
        &self,
        export: &Export,
        entry: &RecoveryRegistration,
    ) -> Result<Option<RecoveryTarget>> {
        if let Some(target) = self
            .check_context_recovery_target(export, &entry.allocation.storage_key)
            .await?
        {
            return Ok(target);
        }
        self.check_first_recovery_target(entry).await
    }
    async fn registered_first(
        &self,
        entry: &RecoveryRegistration,
    ) -> Result<Option<FirstMessageReservation>> {
        let Some(proposed) = &entry.first else {
            return Ok(None);
        };
        let winner = self
            .store
            .first_message_reservation(&proposed.identity, &proposed.fingerprint)
            .await?;
        Ok(winner.filter(|winner| winner.allocation == entry.allocation))
    }

    async fn service_local_owner(
        &self,
        export: &Export,
        context: &ContextSnapshot,
    ) -> Result<bool> {
        let active = context
            .document
            .state
            .active
            .as_ref()
            .context("supervised active task missing")?;
        let slot = self.slot(&ContextKey::new(export, &context.document.local_id));
        // Never wait behind a local admission gate to decide distributed ownership.
        let handle = slot
            .active
            .try_lock()
            .ok()
            .and_then(|active| active.clone());
        let Some(handle) = handle.filter(|h| h.owns_context(context)) else {
            return Ok(false);
        };
        if !handle.lease.revalidate_ownership().await? {
            return Ok(false);
        }
        if active.cancel.is_some() {
            handle
                .session
                .interrupt_admitted_invocation("A2A cancellation")
                .await?;
        }
        Ok(true)
    }
}

impl RunnerHandle {
    fn owns_context(&self, context: &ContextSnapshot) -> bool {
        if *self.done.borrow() {
            return false;
        }
        if context.document.owner.as_ref() != Some(&self.fence) {
            return false;
        }
        context
            .document
            .state
            .active
            .as_ref()
            .is_some_and(|active| active.snapshot.task.id == self.task_id)
    }
}
