//! Registry entries are hints. Context CAS and fixed runtime identity decide work.
use super::supervision::SupervisionConfig;
use super::*;
use crate::store::{
    context::ContextSnapshot, registry::RecoveryRegistration, FirstMessageReservation,
};

enum RecoveryTarget {
    Active,
    First(Box<FirstMessageReservation>),
}

impl Runner {
    pub(super) async fn reconcile_registration(
        &self,
        export: &Export,
        entry: &RecoveryRegistration,
        settings: &SupervisionConfig,
    ) -> Result<()> {
        entry.validate_export(export)?;
        let storage = &entry.allocation.storage_key;
        if self.store.prune_unused_registration(entry).await? {
            return Ok(());
        }
        let Some(target) = self.recovery_target(export, entry).await? else {
            return Ok(());
        };
        let Some(lease) = self.candidate_lease(storage).await? else {
            return Ok(());
        };
        let owner = entry
            .owner
            .clone()
            .map(Principal::User)
            .unwrap_or(Principal::Anonymous);
        let request = super::stored_session::StoredSessionRequest {
            export,
            owner: &owner,
            global_config: &settings.config,
            activation_route: settings.route.clone(),
            abort: settings.abort.clone(),
            allow_create: matches!(&target, RecoveryTarget::First(_)),
        };
        // Recovery uses immutable owner/export facts, not a fabricated request
        // identity. Revocation must deny new requests without preventing stop.
        let session = self.stored_session(request, &entry.allocation).await?;
        if let RecoveryTarget::First(first) = target {
            self.recover_first_reservation(&session, &first, lease)
                .await?;
        } else {
            let mut authority = self.claim_context(&session, lease).await?;
            if authority.context.document.state.active.is_some() {
                self.recover_admission(&session, &mut authority).await?;
            }
            authority.release(&self.store, storage).await?;
        }
        Ok(())
    }

    async fn recovery_target(
        &self,
        export: &Export,
        entry: &RecoveryRegistration,
    ) -> Result<Option<RecoveryTarget>> {
        if let Some(context) = self
            .store
            .read_context(&entry.allocation.storage_key)
            .await?
        {
            crate::diagnostics::pending(&context);
            self.store
                .cleanup_context_events(&entry.allocation.storage_key)
                .await?;
            if let Some(active) = &context.document.state.active {
                if active.ready_to_retire() {
                    return Ok(None);
                }
                if self.service_local_owner(export, &context).await? {
                    return Ok(None);
                }
                return Ok(Some(RecoveryTarget::Active));
            }
        }
        // Claim can land before task allocation. Existing empty authority isn't
        // proof that the retained first reservation finished (or never existed).
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
