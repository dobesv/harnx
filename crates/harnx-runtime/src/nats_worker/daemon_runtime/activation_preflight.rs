//! What an activation has to satisfy before a worker claims the session:
//! canonical metadata, the right route, and — for a session whose last turn
//! was interrupted — winding that turn up before anything else may run.
use super::*;
use harnx_core::session::SessionLogEntry;
use harnx_core::session_reconstruct::TurnStatus;

struct MetadataPreflightFailure<'a> {
    delivery: &'a mut ActivationDelivery,
    activation: &'a SessionActivate,
    generation: u64,
    error: anyhow::Error,
}
impl WorkerRuntime {
    async fn metadata_preflight_passes(
        &self,
        delivery: &mut ActivationDelivery,
        activation: &SessionActivate,
        generation: u64,
    ) -> Result<bool> {
        match self.session_metadata.get(&activation.session_id).await {
            Ok(Some(_)) => Ok(true),
            Ok(None) => {
                log::warn!(
                    "terminating SessionActivate without canonical metadata: session_id={}",
                    activation.session_id
                );
                self.terminate_activation(delivery, "metadata-less").await?;
                Ok(false)
            }
            Err(error) => {
                self.handle_metadata_preflight_failure(MetadataPreflightFailure {
                    delivery,
                    activation,
                    generation,
                    error,
                })
                .await
            }
        }
    }

    async fn retry_metadata_preflight_failure(
        &self,
        delivery: &mut ActivationDelivery,
        error: anyhow::Error,
    ) -> Result<bool> {
        if self.uses_targeted_activation() {
            Self::delayed_nak(delivery, super::ActivationNakReason::PreflightNotReady).await?;
            Ok(false)
        } else {
            Err(error)
        }
    }

    async fn handle_metadata_preflight_failure(
        &self,
        failure: MetadataPreflightFailure<'_>,
    ) -> Result<bool> {
        let MetadataPreflightFailure {
            delivery,
            activation,
            generation,
            error,
        } = failure;
        log::warn!(
            "session metadata preflight failed for '{}': {error:#}",
            activation.session_id
        );
        if self.shutdown.is_cancelled() {
            Self::shutdown_nak(delivery).await?;
            return Ok(false);
        }
        let failures = match self
            .count_activation_failure(activation, delivery.failure_key())
            .await
        {
            Ok(failures) => failures,
            Err(counter_error) => {
                log::warn!(
                    "activation failure counter unavailable for '{}': {counter_error:#}",
                    activation.session_id
                );
                return self.retry_metadata_preflight_failure(delivery, error).await;
            }
        };
        if failures < super::super::daemon_session_exec::MAX_ACTIVATION_FAILURES {
            return self.retry_metadata_preflight_failure(delivery, error).await;
        }
        let Some(lease) = self
            .acquire_or_defer_activation(delivery, activation, generation)
            .await?
        else {
            return Ok(false);
        };
        delivery.attach_lease(&lease);
        let error = self
            .durabilize_pre_turn_failure(activation, &lease, error)
            .await;
        if super::super::daemon_session_exec::is_durable_activation_error(&error) {
            self.terminate_activation(delivery, "durably failed")
                .await?;
        } else {
            Self::delayed_nak(delivery, super::ActivationNakReason::PreflightNotReady).await?;
        }
        lease.release().await?;
        Ok(false)
    }

    async fn targeted_route_preflight_passes(
        &self,
        delivery: &mut ActivationDelivery,
        activation: &SessionActivate,
    ) -> Result<bool> {
        if !self.uses_targeted_activation() {
            return Ok(true);
        }
        if let Err(error) = self.validate_targeted_activation(activation) {
            log::warn!("terminating misrouted targeted SessionActivate: {error:#}");
            self.terminate_activation(delivery, "misrouted targeted")
                .await?;
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
        delivery: &mut ActivationDelivery,
        activation: &SessionActivate,
        generation: u64,
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
                self.acknowledge_activation(delivery, "wound-up interruption")
                    .await?;
                return Ok(false);
            }
            _ => return Ok(true),
        }
        let Some(lease) = self
            .acquire_or_defer_activation(delivery, activation, generation)
            .await?
        else {
            return Ok(false);
        };
        delivery.attach_lease(&lease);
        if self.shutdown.is_cancelled() {
            lease.release().await?;
            Self::shutdown_nak(delivery).await?;
            return Ok(false);
        }
        let wound_up = self.wind_up_interrupted_session(&backend, &lease).await;
        if lease.is_held() {
            // This lease protected wind-up only. Keep heartbeating while this
            // activation either settles or acquires its execution lease.
            delivery.detach_lease();
        }
        lease.release().await?;
        wound_up?;
        if !state.next_turn_messages.is_empty() {
            // Input typed after the interrupt: the activation still has a turn
            // to run, now that the interrupted one is closed out.
            return Ok(true);
        }
        self.acknowledge_activation(delivery, "wind-up").await?;
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

    async fn lease_holder_preflight_passes(
        &self,
        delivery: &mut ActivationDelivery,
        activation: &SessionActivate,
    ) -> Result<bool> {
        match crate::nats_lease::lease_holder_in(
            &self.lease_bucket,
            &self.lease,
            &activation.session_id,
        )
        .await
        {
            Ok(Some(holder)) => {
                log::debug!(
                    "deferring busy activation before session preflight: session_id={} holder_worker_id={} holder_generation={}",
                    activation.session_id,
                    holder.worker_id,
                    holder.generation
                );
                Self::busy_nak(delivery).await?;
                Ok(false)
            }
            Ok(None) => Ok(true),
            Err(error) => {
                log::warn!(
                    "activation lease-holder preflight failed for '{}': {error:#}",
                    activation.session_id
                );
                if self.uses_targeted_activation() {
                    Self::delayed_nak(delivery, super::ActivationNakReason::PreflightNotReady)
                        .await?;
                    Ok(false)
                } else {
                    Err(error)
                }
            }
        }
    }

    pub(super) async fn activation_is_ready(
        &self,
        delivery: &mut ActivationDelivery,
        activation: &SessionActivate,
        generation: u64,
    ) -> Result<bool> {
        if !self
            .targeted_route_preflight_passes(delivery, activation)
            .await?
        {
            return Ok(false);
        }
        // This cached KV read avoids metadata and full-log reads while another
        // owner is live. Lease acquisition below remains the ownership CAS.
        if !self
            .lease_holder_preflight_passes(delivery, activation)
            .await?
        {
            return Ok(false);
        }
        if !self
            .metadata_preflight_passes(delivery, activation, generation)
            .await?
        {
            return Ok(false);
        }

        if self.shutdown.is_cancelled() {
            Self::shutdown_nak(delivery).await?;
            return Ok(false);
        }

        if !self
            .wind_up_preflight(delivery, activation, generation)
            .await?
        {
            return Ok(false);
        }

        if self
            .activation_status_preflight_finished(delivery, activation)
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
