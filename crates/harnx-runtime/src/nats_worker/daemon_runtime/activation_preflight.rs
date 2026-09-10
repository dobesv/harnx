//! Activation admission and recovery before claiming an execution owner.
use super::*;

impl WorkerRuntime {
    async fn execution_preflight_passes(
        &self,
        message: &async_nats::jetstream::Message,
        activation: &SessionActivate,
    ) -> Result<bool> {
        let Some(expected) = activation.execution_id.as_ref() else {
            return Ok(true);
        };
        let store =
            harnx_execution_control::ExecutionStore::ensure(&self.jetstream, self.lease.replicas)
                .await?;
        let Some(current) = store.current(&activation.session_id).await? else {
            return Ok(true);
        };
        let current = store.status(&current.reference).await?;
        if current.reference.execution_id == *expected
            && current.owner_stopped
            && !current.state.is_terminal()
            && !self.has_uncovered_admission(&store, &current).await?
        {
            Self::delayed_nak(message).await?;
            return Ok(false);
        }
        if current.reference.execution_id != *expected || current.state.is_terminal() {
            if current.reference.execution_id == *expected {
                self.end_session_tool_servers(&activation.session_id).await;
            }
            message
                .ack()
                .await
                .map_err(|error| anyhow::anyhow!("ack stale execution: {error}"))?;
            return Ok(false);
        }
        Ok(true)
    }

    async fn has_uncovered_admission(
        &self,
        store: &harnx_execution_control::ExecutionStore,
        operation: &harnx_execution_control::Operation,
    ) -> Result<bool> {
        let log =
            NatsSessionLogBackend::new(self.jetstream.clone(), &operation.reference.session_id);
        let entries = log.load_events_latest_async().await?;
        crate::nats_session::cancellation::reconcile_admissions(store, operation, &entries).await?;
        let operation = store
            .get(&operation.reference)
            .await?
            .context("execution missing")?;
        if operation.state.cancelling() && !operation.cancel_recorded {
            return Ok(true);
        }
        Ok(operation
            .admissions
            .values()
            .filter_map(|seq| *seq)
            .any(|seq| seq > operation.covered_through))
    }

    async fn metadata_preflight_passes(
        &self,
        message: &async_nats::jetstream::Message,
        activation: &SessionActivate,
    ) -> Result<bool> {
        match self.session_metadata.get(&activation.session_id).await {
            Ok(Some(_)) => Ok(true),
            Ok(None) => {
                log::warn!(
                    "terminating SessionActivate without canonical metadata: session_id={}",
                    activation.session_id
                );
                Self::terminate_activation(message, "metadata-less").await?;
                Ok(false)
            }
            Err(error) => {
                log::warn!(
                    "session metadata preflight failed for '{}': {error:#}",
                    activation.session_id
                );
                if self.uses_targeted_activation() {
                    Self::delayed_nak(message).await?;
                    Ok(false)
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn targeted_route_preflight_passes(
        &self,
        message: &async_nats::jetstream::Message,
        activation: &SessionActivate,
    ) -> Result<bool> {
        if !self.uses_targeted_activation() {
            return Ok(true);
        }
        if let Err(error) = self.validate_targeted_activation(activation) {
            log::warn!("terminating misrouted targeted SessionActivate: {error:#}");
            Self::terminate_activation(message, "misrouted targeted").await?;
            return Ok(false);
        }
        Ok(true)
    }

    pub(super) async fn activation_is_ready(
        &self,
        message: &async_nats::jetstream::Message,
        activation: &SessionActivate,
    ) -> Result<bool> {
        if !self.metadata_preflight_passes(message, activation).await? {
            return Ok(false);
        }
        if !self
            .targeted_route_preflight_passes(message, activation)
            .await?
        {
            return Ok(false);
        }

        if !self.execution_preflight_passes(message, activation).await? {
            return Ok(false);
        }

        // A targeted re-activation stays durable until the active loop's tool
        // boundary or final drain has covered the requested sequence.
        if self.already_running(&activation.session_id).await {
            self.settle_running_activation(message).await?;
            return Ok(false);
        }

        if activation.execution_id.is_none()
            && self.uses_targeted_activation()
            && self
                .targeted_status_preflight_finished(message, activation)
                .await?
        {
            return Ok(false);
        }

        Ok(true)
    }
}
