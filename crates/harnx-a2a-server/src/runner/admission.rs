//! Scoped lease selects a candidate; same-document CAS admits immutable work.
use super::authority::{OwnedTask, TaskReservation};
use super::*;
use crate::store::{context::ContextIdentity, MessageIdentity, TaskAllocation};
use harnx_runtime::{
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session::fixed_admission::FixedAdmissionTicket,
};

pub(crate) struct AllocatedAdmission {
    pub allocation: TaskAllocation,
    pub lease: Arc<NatsSessionLease>,
    pub fixed_predecessor: Option<u64>,
}

impl Runner {
    pub(crate) async fn candidate_lease(
        &self,
        storage: &str,
    ) -> Result<Option<Arc<NatsSessionLease>>> {
        Ok(NatsSessionLease::acquire_scoped(
            NatsLeaseAcquireParams {
                jetstream: self.store.metadata().jetstream().clone(),
                session_id: storage,
                worker_id: self.boot_id.clone(),
                generation: 1,
                config: self.coordination_lease_config(),
                session_metadata: None,
            },
            "a2a",
        )
        .await?
        .map(Arc::new))
    }
    pub(super) fn coordination_lease_config(&self) -> NatsLeaseConfig {
        #[cfg(feature = "fault-injection")]
        if let Some(config) = self.lease_config_override.lock().clone() {
            return config;
        }
        NatsLeaseConfig {
            replicas: self.store.metadata().replicas(),
            ..Default::default()
        }
    }
    #[cfg(feature = "fault-injection")]
    pub fn set_lease_config_for_test(&self, config: NatsLeaseConfig) {
        *self.lease_config_override.lock() = Some(config);
    }

    pub(super) async fn claim_context(
        &self,
        session: &NatsSession,
        lease: Arc<NatsSessionLease>,
    ) -> Result<OwnedTask> {
        if self
            .store
            .read_context(session.storage_key())
            .await?
            .is_none()
        {
            anyhow::ensure!(
                self.store
                    .list_non_terminal_entries(session.storage_key())
                    .await?
                    .is_empty(),
                RunnerError::Busy
            );
        }
        if self
            .store
            .read_context(session.storage_key())
            .await?
            .is_none()
        {
            // Runtime's short-ID allocator may reserve metadata without a log.
            // Bootstrap once before first authority activation, never during
            // recovery of an existing fixed ticket or a deleted authority.
            harnx_runtime::nats_session_log::NatsSessionLog::new_with_replicas(
                self.store.metadata().jetstream().clone(),
                session.storage_key(),
                self.store.metadata().replicas(),
            )
            .load_events_latest_async()
            .await?;
        }
        let write = self
            .store
            .prepare_context_claim(
                ContextIdentity {
                    storage_key: session.storage_key(),
                    local_id: session.session_id(),
                },
                &lease,
                &uuid::Uuid::new_v4().to_string(),
            )
            .await?;
        let context = self.store.commit_context(&write).await?;
        if context.document.epoch > 1
            && context
                .document
                .state
                .active
                .as_ref()
                .is_some_and(|active| !active.ready_to_retire())
        {
            metrics::counter!("harnx_a2a_owner_lost_total").increment(1);
        }
        Ok(OwnedTask { lease, context })
    }

    /// Existing callers still use this API. Local slots are only a cache.
    pub(crate) async fn start_turn_with_input(
        self: &Arc<Self>,
        request: TurnRequest<'_>,
        input: Input,
    ) -> Result<StartTurnResult> {
        self.authorize_session(request.export, request.owner, &request.session)
            .await?;
        let fingerprint = crate::store::message_fingerprint(&request.message.parts);
        if let Some(record) = self.existing_identity(&request, &fingerprint).await? {
            return Ok(retry_result(record));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let lease = loop {
            if let Some(lease) = self.candidate_lease(request.session.storage_key()).await? {
                break lease;
            }
            if let Some(record) = self.existing_identity(&request, &fingerprint).await? {
                return Ok(retry_result(record));
            }
            let current = self
                .store
                .read_context(request.session.storage_key())
                .await?;
            if current
                .and_then(|c| c.document.state.active)
                .is_some_and(|active| !active.snapshot.task.status.state.is_terminal())
                || tokio::time::Instant::now() >= deadline
            {
                return Err(RunnerError::Busy.into());
            }
            // A candidate is still moving from idle claim to reservation. Don't
            // turn the same-message loser into Busy before its identity is visible.
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let allocation =
            TaskAllocation::new(&request.export.agent, request.session.session_id().into());
        self.store
            .register_recovery(request.export, &request.owner.principal, &allocation, None)
            .await?;
        self.start_allocated_turn(
            request,
            input,
            AllocatedAdmission {
                allocation,
                lease,
                fixed_predecessor: None,
            },
        )
        .await
    }

    pub(crate) async fn start_allocated_turn(
        self: &Arc<Self>,
        request: TurnRequest<'_>,
        input: Input,
        admission: AllocatedAdmission,
    ) -> Result<StartTurnResult> {
        let TurnRequest {
            export,
            owner,
            session,
            message,
        } = request;
        self.authorize_session(export, owner, &session).await?;
        let authority = self
            .reserve_active_message(&session, message, &admission)
            .await?;
        let allocation = admission.allocation;
        #[cfg(feature = "fault-injection")]
        self.hooks
            .checkpoint(crate::fault_injection::Boundary::Claim)
            .await;
        let slot = self.slot(&ContextKey::new(export, session.session_id()));
        let active = slot.active.clone().lock_owned().await;
        anyhow::ensure!(
            !self.shutting_down.load(Ordering::SeqCst),
            "A2A runner is shutting down"
        );
        self.launch_turn(
            slot,
            active,
            PendingTurn {
                session,
                input,
                allocation,
                authority,
            },
        )
        .await
    }
}
fn retry_result(snapshot: TaskRecord) -> StartTurnResult {
    let (_, events) = broadcast::channel(EVENT_CAPACITY);
    StartTurnResult {
        snapshot,
        events,
        deduped: true,
    }
}

impl Runner {
    async fn existing_identity(
        &self,
        request: &TurnRequest<'_>,
        fingerprint: &str,
    ) -> Result<Option<TaskRecord>> {
        self.store
            .dedupe_task(
                crate::store::ContextAccess {
                    export: request.export,
                    owner: request.owner,
                    local_id: request.session.session_id(),
                },
                MessageIdentity {
                    message_id: &request.message.message_id,
                    fingerprint,
                },
            )
            .await
    }
    async fn reserve_active_message(
        &self,
        session: &NatsSession,
        message: Message,
        admission: &AllocatedAdmission,
    ) -> Result<OwnedTask> {
        let mut authority = self.claim_context(session, admission.lease.clone()).await?;
        if authority.context.document.state.active.is_some() {
            self.recover_admission(session, &mut authority).await?;
        }
        let predecessor = match admission.fixed_predecessor {
            Some(tail) => tail,
            None => session
                .prepare_fixed_admission(
                    admission.allocation.invocation_id.clone(),
                    admission.allocation.prompt_id.clone(),
                    admission.allocation.closure_id.clone(),
                )
                .await?
                .expected_predecessor(),
        };
        // Construct once; future recovery restores these fields, never a later tail.
        FixedAdmissionTicket::from_parts(
            session.storage_key().into(),
            admission.allocation.invocation_id.clone(),
            admission.allocation.prompt_id.clone(),
            admission.allocation.closure_id.clone(),
            predecessor,
        )?;
        authority
            .allocate(
                &self.store,
                session,
                TaskReservation {
                    allocation: &admission.allocation,
                    message,
                    predecessor,
                },
            )
            .await?;
        Ok(authority)
    }
}
