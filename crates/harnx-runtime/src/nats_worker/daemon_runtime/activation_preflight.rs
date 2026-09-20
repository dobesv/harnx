//! What an activation has to satisfy before a worker claims the session:
//! canonical metadata, the right route, and — for a session whose last turn
//! was interrupted — winding that turn up before anything else may run.
use super::*;
use harnx_core::session::SessionLogEntry;
use harnx_core::session_reconstruct::TurnStatus;

impl WorkerRuntime {
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

    /// Close out an interrupted turn before anything else runs for this
    /// session: the log owes a result for every call its `Cancel` cut off, and
    /// a transcript whose last `ToolCalls` is unanswered cannot go to a model.
    ///
    /// Returns whether the activation still has a turn to run. A pure wind-up
    /// is acknowledged here and goes no further; a wind-up with a steering
    /// message queued behind the `Cancel` continues into the turn loop, which
    /// now sees an idle session with pending input.
    async fn wind_up_preflight(
        &self,
        message: &async_nats::jetstream::Message,
        activation: &SessionActivate,
    ) -> Result<bool> {
        let backend = NatsSessionLogBackend::new(
            self.jetstream.clone(),
            &activation.session_id,
            self.lease.replicas,
        );
        let entries = backend.load_events_latest_async().await?;
        let state = harnx_core::session_reconstruct::reconstruct_state_from_nats(&entries);
        match state.turn_status {
            TurnStatus::InterruptedPendingWindUp { .. } => {}
            // The interruption this activation was published for has already
            // been wound up; there is nothing left for it to do.
            TurnStatus::Idle if cancel_landed_at(&entries, activation.requested_seq) => {
                Self::acknowledge(message, "wound-up interruption").await?;
                return Ok(false);
            }
            _ => return Ok(true),
        }
        let Some(lease) = self
            .acquire_or_defer_activation(message, activation)
            .await?
        else {
            return Ok(false);
        };
        if self.shutdown.is_cancelled() {
            lease.release().await?;
            Self::shutdown_nak(message).await?;
            return Ok(false);
        }
        let wound_up = self.wind_up_interrupted_session(&backend, &lease).await;
        lease.release().await?;
        wound_up?;
        if !state.next_turn_messages.is_empty() {
            // Input typed after the interrupt: the activation still has a turn
            // to run, now that the interrupted one is closed out.
            return Ok(true);
        }
        Self::acknowledge(message, "wind-up").await?;
        Ok(false)
    }

    async fn wind_up_interrupted_session(
        &self,
        backend: &NatsSessionLogBackend,
        lease: &NatsSessionLease,
    ) -> Result<()> {
        let event_sink = crate::nats_event_sink::NatsEventSink::new(
            self.client.clone(),
            self.jetstream.clone(),
            backend.session_id().to_string(),
        )
        .await;
        let outcome =
            super::super::wind_up::wind_up_interrupted_turn(super::super::wind_up::WindUpInputs {
                backend,
                lease,
                client: &self.client,
                jetstream: &self.jetstream,
                replicas: self.lease.replicas,
                in_flight: &crate::nats_tool_provider::NatsInFlightCalls::for_instance(
                    &self.instance_id,
                ),
                event_sink: Some(&event_sink),
            })
            .await?;
        log::info!(
            "activation wound up an interrupted turn: session_id={} worker_id={} outcome={outcome:?}",
            backend.session_id(),
            self.worker_id,
        );
        Ok(())
    }

    async fn acknowledge(message: &async_nats::jetstream::Message, reason: &str) -> Result<()> {
        message
            .ack()
            .await
            .map_err(|error| anyhow::anyhow!("ack {reason} SessionActivate: {error}"))
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

        // A targeted re-activation stays durable until the active loop's tool
        // boundary or final drain has covered the requested sequence. Checked
        // before wind-up so a running turn's lease is never taken from it.
        let already_running = self.already_running(&activation.session_id).await;
        if self.shutdown.is_cancelled() {
            Self::shutdown_nak(message).await?;
            return Ok(false);
        }
        if already_running {
            self.settle_running_activation(message).await?;
            return Ok(false);
        }

        if !self.wind_up_preflight(message, activation).await? {
            return Ok(false);
        }

        if self.uses_targeted_activation()
            && self
                .targeted_status_preflight_finished(message, activation)
                .await?
        {
            return Ok(false);
        }

        Ok(true)
    }
}

/// Whether the sequence this activation names is a `Cancel` still in the log.
fn cancel_landed_at(entries: &[(u64, SessionLogEntry)], requested_seq: Option<u64>) -> bool {
    let Some(requested_seq) = requested_seq else {
        return false;
    };
    entries.iter().any(|(seq, entry)| {
        *seq == requested_seq && matches!(entry, SessionLogEntry::Cancel { .. })
    })
}
