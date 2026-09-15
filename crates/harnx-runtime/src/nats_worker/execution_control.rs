use super::{NatsSessionLogBackend, SessionActivate};
use crate::nats_lease::NatsSessionLease;
use anyhow::{Context, Result};
use harnx_core::session::SessionLogEntry;
use harnx_execution_control::{ExecutionStore, OperationRef, OperationState, Owner};

#[derive(Clone)]
pub(super) struct WorkerExecution {
    pub store: ExecutionStore,
    pub reference: OperationRef,
    pub owner: Owner,
    pub fence: Option<crate::execution_fence::GenerationFence>,
    /// A recovery-only worker cannot confirm resources owned by a prior process.
    owns_turn: bool,
}

impl WorkerExecution {
    pub async fn claim(
        store: ExecutionStore,
        activation: &mut SessionActivate,
        lease: &NatsSessionLease,
        jetstream: &async_nats::jetstream::Context,
    ) -> Result<Self> {
        let operation = resolve_activation(&store, activation, jetstream).await?;
        activation.execution_id = Some(operation.reference.execution_id.clone());
        if let Some(registration) = &operation.gate_registration {
            if store
                .gate_stop(registration.context.gate_root(), &operation.reference)
                .await?
                .is_some()
            {
                let context = store
                    .gate_context(registration.context.gate_root(), &operation.reference)
                    .await?;
                return Ok(Self {
                    owner: context.owner().clone(),
                    reference: operation.reference,
                    fence: Some(crate::execution_fence::GenerationFence::new(
                        store.clone(),
                        context,
                    )),
                    store,
                    owns_turn: false,
                });
            }
        }
        let mut execution = Self {
            owns_turn: true,
            fence: None,
            store,
            reference: operation.reference,
            owner: Owner {
                instance_id: lease.worker_id().into(),
                fence: lease.fence_token(),
            },
        };
        let claimed = execution
            .store
            .claim(&execution.reference, execution.owner.clone())
            .await?;
        execution
            .prepare_recovery(&claimed, activation, jetstream)
            .await?;
        Ok(execution)
    }

    async fn prepare_recovery(
        &mut self,
        claimed: &harnx_execution_control::Operation,
        activation: &SessionActivate,
        jetstream: &async_nats::jetstream::Context,
    ) -> Result<()> {
        // Open at worker claim, before constructing any reducer, hook, or sink.
        // A legacy operation cancelled before activation remains cleanup-only.
        if claimed.allows_continuation() || claimed.gate_registration.is_some() {
            let context = Box::pin(self.store.activate_gate(&self.reference)).await?;
            anyhow::ensure!(
                context.owner() == &self.owner,
                "worker gate owner changed during claim"
            );
            self.fence = Some(crate::execution_fence::GenerationFence::new(
                self.store.clone(),
                context,
            ));
        }
        if let Some(fence) = &self.fence {
            let log = crate::nats_session_log::NatsSessionLog::new(
                jetstream.clone(),
                &activation.session_id,
            );
            match log.admit_reconstruction(fence).await {
                Err(error)
                    if error.is::<harnx_execution_control::Interrupted>()
                        && self.cancelled().await? => {}
                result => result?,
            }
        }
        Ok(())
    }

    pub async fn cancelled(&self) -> Result<bool> {
        if let Some(fence) = &self.fence {
            if self
                .store
                .gate_stop(fence.context.gate_root(), &self.reference)
                .await?
                .is_some()
            {
                fence.stop_events();
                return Ok(true);
            }
        }
        Ok(self
            .store
            .get(&self.reference)
            .await?
            .context("execution missing")?
            .state
            .cancelling())
    }

    pub async fn cover_turn(&self, through: u64) -> Result<()> {
        self.store
            .record_coverage(&self.reference, &self.owner, through, false)
            .await
    }

    pub async fn record_cancel(
        &self,
        backend: &NatsSessionLogBackend,
        lease: &NatsSessionLease,
    ) -> Result<()> {
        self.store
            .cancel_operation(&self.reference, None, false)
            .await?;
        self.store.quiesce(&self.reference, &self.owner).await?;
        if !lease.is_held() {
            anyhow::bail!("cannot record cancellation after lease loss");
        }
        let entries = backend.load_events_latest_async().await?;
        let operation = self
            .store
            .get(&self.reference)
            .await?
            .context("execution missing")?;
        crate::nats_session::cancellation::reconcile_admissions(&self.store, &operation, &entries)
            .await?;
        let seq = backend
            .append_event(&SessionLogEntry::Cancel {
                fence_token: lease.fence_token(),
            })
            .await?;
        self.store
            .record_coverage(&self.reference, &self.owner, seq, true)
            .await
    }

    pub async fn finish(
        &self,
        backend: &NatsSessionLogBackend,
        lease: &NatsSessionLease,
        turn_config: crate::config::GlobalConfig,
        turn: Option<tokio::task::JoinHandle<Result<()>>>,
    ) -> Result<bool> {
        let local_abort = turn_config
            .read()
            .maintenance_abort
            .as_ref()
            .is_some_and(|signal| signal.aborted());
        if local_abort {
            let _ = self
                .store
                .cancel_operation(&self.reference, None, false)
                .await;
        }
        let cancelled = local_abort || self.cancelled().await?;
        let projection = if cancelled && lease.is_held() {
            self.record_cancel(backend, lease).await
        } else {
            Ok(())
        };
        lease.release().await?;
        if cancelled {
            let execution = self.clone();
            harnx_execution_control::CleanupTasks::process().spawn(async move {
                // G1-only state, no lease handle or session dispatcher authority.
                if let Some(turn) = turn {
                    let _ = turn.await;
                }
                while turn_config
                    .read()
                    .session
                    .as_ref()
                    .is_some_and(|session| session.compressing() || session.titling())
                {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                let result = match &execution.fence {
                    Some(fence) if !execution.owns_turn => {
                        execution
                            .store
                            .record_cleanup(
                                &fence.context,
                                harnx_execution_control::CleanupStatus::unconfirmed(
                                    "prior worker resource handles unavailable after recovery",
                                ),
                            )
                            .await
                    }
                    Some(fence) => execution.store.finish_cleanup_owner(&fence.context).await,
                    None => execution
                        .store
                        .owner_stopped(&execution.reference, &execution.owner)
                        .await
                        .map(drop),
                };
                if let Err(error) = result {
                    log::warn!("session cleanup evidence unconfirmed: {error:#}");
                }
            });
            projection?;
            return Ok(true);
        }
        let operation = self
            .store
            .owner_stopped(&self.reference, &self.owner)
            .await?;
        // Nonterminal work leaves the activation unacknowledged, so lease
        // expiry/replacement can finish the durable cancellation even if this
        // worker or the requester disappears.
        Ok(matches!(
            operation.state,
            OperationState::Completed | OperationState::Cancelled
        ))
    }
}

async fn resolve_activation(
    store: &ExecutionStore,
    activation: &SessionActivate,
    jetstream: &async_nats::jetstream::Context,
) -> Result<harnx_execution_control::Operation> {
    let recovered = if activation.execution_id.is_none() {
        crate::nats_session::cancellation::resolve_pending_execution(
            store,
            jetstream,
            &activation.session_id,
        )
        .await?
    } else {
        None
    };
    let operation = match recovered.or(store.current(&activation.session_id).await?) {
        Some(operation) => operation,
        None => {
            store
                .session(
                    &activation.session_id,
                    None,
                    activation.execution_id.as_deref(),
                )
                .await?
        }
    };
    if let Some(expected) = &activation.execution_id {
        anyhow::ensure!(
            expected == &operation.reference.execution_id,
            "stale execution activation"
        );
    }
    if operation.gate_registration.is_some() {
        crate::nats_session_log::NatsSessionLog::new(jetstream.clone(), &activation.session_id)
            .recover_stop(store, &operation.reference)
            .await?;
    }
    Ok(operation)
}
