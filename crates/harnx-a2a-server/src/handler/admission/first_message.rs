use super::*;
use crate::{
    identity::Principal,
    runner::SessionRequest,
    store::{FirstMessageReservation, TaskAllocation},
};
use harnx_runtime::{nats_lease::NatsSessionLease, NatsSession};
use std::sync::Arc;
use std::time::Duration;

struct FirstCandidate {
    reservation: FirstMessageReservation,
    lease: Arc<NatsSessionLease>,
    own: bool,
}

impl HarnxHandler {
    pub(super) async fn admit_first_message(
        &self,
        owner: &RequestIdentity,
        request: MessageAdmission,
    ) -> Result<Admission, A2AError> {
        let input = message_to_input(&request.message, self.limits)
            .map_err(|error| map_error(error.into()))?;
        let FirstCandidate {
            reservation,
            lease,
            own,
        } = self.reserve_first_candidate(request).await?;
        if !own {
            lease.release().await.map_err(map_error)?;
            return self
                .follow_first_reservation(owner, &reservation)
                .await
                .map(Admission::retry);
        }
        #[cfg(feature = "fault-injection")]
        self.backend
            .runner
            .fault_hooks()
            .checkpoint(crate::fault_injection::Boundary::FirstReservation)
            .await;
        if !lease.revalidate_ownership().await.map_err(map_error)? {
            return self
                .follow_first_reservation(owner, &reservation)
                .await
                .map(Admission::retry);
        }
        let session = self
            .initialize_reserved_session(owner, &reservation)
            .await?;
        #[cfg(feature = "fault-injection")]
        self.backend
            .runner
            .fault_hooks()
            .checkpoint(crate::fault_injection::Boundary::SessionInitialized)
            .await;
        let started = self
            .backend
            .runner
            .start_allocated_turn(
                TurnRequest {
                    export: &self.export,
                    owner,
                    session,
                    message: reservation.message.clone(),
                },
                input,
                crate::runner::AllocatedAdmission {
                    allocation: reservation.allocation.clone(),
                    lease,
                    fixed_predecessor: Some(0),
                },
            )
            .await
            .map_err(map_error)?;
        Ok(Admission {
            snapshot: started.snapshot,
            deduped: false,
        })
    }
    pub(super) async fn follow_first_reservation(
        &self,
        owner: &RequestIdentity,
        reservation: &FirstMessageReservation,
    ) -> Result<TaskRecord, A2AError> {
        let allocation = &reservation.allocation;
        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(record) = self
                    .backend
                    .store
                    .get_task(&allocation.storage_key, &allocation.task_id)
                    .await?
                {
                    // Re-check binding on every retained identity, never trust LRU.
                    self.backend
                        .store
                        .resolve_context(&self.export, owner, &allocation.local_id)
                        .await?
                        .ok_or(crate::store::StoreError::NotFound)?;
                    return self
                        .reconcile(owner, record)
                        .await
                        .map_err(anyhow::Error::from);
                }
                if let Some(lease) = self
                    .backend
                    .runner
                    .candidate_lease(&allocation.storage_key)
                    .await?
                {
                    let session = self
                        .initialize_reserved_session(owner, reservation)
                        .await
                        .map_err(anyhow::Error::from)?;
                    return self
                        .backend
                        .runner
                        .recover_first_reservation(&session, reservation, lease)
                        .await;
                }
                // Poll persisted authority during initialization. Absence plus a
                // held candidate lease is NOT evidence of a crashed owner.
                #[cfg(feature = "fault-injection")]
                self.backend
                    .runner
                    .fault_hooks()
                    .checkpoint(crate::fault_injection::Boundary::InitializationWait)
                    .await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .map_err(|_| {
            A2AError::internal(
                "message reservation is still initializing; retry with the same messageId",
            )
        })?;
        outcome.map_err(map_error)
    }
    async fn initialize_reserved_session(
        &self,
        owner: &RequestIdentity,
        reservation: &FirstMessageReservation,
    ) -> Result<NatsSession, A2AError> {
        // Admin permission doesn't let a request adopt another principal's
        // first-message reservation. Membership changes don't change its scope.
        let actual = (
            reservation.identity.owner.as_deref(),
            reservation.identity.export.as_str(),
            reservation.identity.cluster.as_str(),
        );
        let expected = (
            owner.principal.user_id(),
            self.export.public_name.as_str(),
            self.export.cluster.as_deref().unwrap_or("__local__"),
        );
        if actual != expected {
            return Err(not_found());
        }
        self.backend
            .runner
            .allocated_session(
                SessionRequest {
                    export: &self.export,
                    owner,
                    local_id: None,
                    global_config: &self.backend.config,
                    activation_route: self.backend.session_route().await.map_err(map_error)?,
                    abort: self.backend.abort.clone(),
                },
                &reservation.allocation,
            )
            .await
            .map_err(map_error)
    }
}

impl HarnxHandler {
    async fn reserve_first_candidate(
        &self,
        request: MessageAdmission,
    ) -> Result<FirstCandidate, A2AError> {
        let allocation = TaskAllocation::new(
            &self.export.agent,
            uuid::Uuid::new_v4().simple().to_string(),
        );
        let lease = self
            .backend
            .runner
            .candidate_lease(&allocation.storage_key)
            .await
            .map_err(map_error)?
            .ok_or_else(|| map_error(RunnerError::Busy.into()))?;
        let proposed = FirstMessageReservation {
            identity: request.lru_key,
            fingerprint: request.fingerprint,
            message: request.message.clone(),
            allocation,
        };
        self.backend
            .store
            .register_recovery(
                &self.export,
                &proposed
                    .identity
                    .owner
                    .clone()
                    .map(Principal::User)
                    .unwrap_or(Principal::Anonymous),
                &proposed.allocation,
                Some(proposed.clone()),
            )
            .await
            .map_err(map_error)?;
        let reserved = self
            .backend
            .store
            .reserve_first_message(&proposed, &lease)
            .await;
        let (reservation, own) = match reserved {
            Ok(reserved) => reserved,
            Err(error) => {
                let _ = lease.release().await;
                return Err(map_error(error));
            }
        };
        Ok(FirstCandidate {
            reservation,
            lease,
            own,
        })
    }
}
