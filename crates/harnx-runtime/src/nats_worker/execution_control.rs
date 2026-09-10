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
}

impl WorkerExecution {
    pub async fn claim(
        store: ExecutionStore,
        activation: &mut SessionActivate,
        lease: &NatsSessionLease,
        jetstream: &async_nats::jetstream::Context,
    ) -> Result<Self> {
        if activation.execution_id.is_none() {
            crate::nats_session::cancellation::resolve_pending_execution(
                &store,
                jetstream,
                &activation.session_id,
            )
            .await?;
        }
        let operation = match store.current(&activation.session_id).await? {
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
        activation.execution_id = Some(operation.reference.execution_id.clone());
        let execution = Self {
            store,
            reference: operation.reference,
            owner: Owner {
                instance_id: lease.worker_id().into(),
                fence: lease.fence_token(),
            },
        };
        execution
            .store
            .claim(&execution.reference, execution.owner.clone())
            .await?;
        Ok(execution)
    }

    pub async fn cancelled(&self) -> Result<bool> {
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
    ) -> Result<bool> {
        if self.cancelled().await? {
            self.record_cancel(backend, lease).await?;
        }
        lease.release().await?;
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
