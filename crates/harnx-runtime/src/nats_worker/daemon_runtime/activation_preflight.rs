//! What an activation has to satisfy before a worker claims the session:
//! canonical metadata, the right route, and — for a session whose last turn
//! was interrupted — winding that turn up before anything else may run.
use super::*;
use crate::nats_lease::{LeaseAssessment, ReclaimReason};
use anyhow::Context as _;
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
        let lease = match self
            .acquire_or_defer_activation(delivery, activation, generation)
            .await?
        {
            LeaseAcquisition::Acquired(lease) => lease,
            LeaseAcquisition::Held => {
                // Reached only after the failure budget is spent: another
                // holder owns the session, so hand this delivery back for a
                // redelivery.
                Self::busy_nak(delivery).await?;
                return Ok(false);
            }
            // A targeted acquisition error already NAKed the delivery back.
            LeaseAcquisition::Settled => return Ok(false),
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
        )
        // Wind-up writes journal replies the interrupted turn never appended,
        // so its `ToolResults` entry needs the size bound's owner too.
        .with_metadata_store(Some(self.session_metadata.clone()));
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
        let lease = match self
            .acquire_or_defer_activation(delivery, activation, generation)
            .await?
        {
            LeaseAcquisition::Acquired(lease) => lease,
            LeaseAcquisition::Held => {
                // Another holder won the lease while the interruption waited
                // to be wound up; hand the delivery back for a redelivery.
                Self::busy_nak(delivery).await?;
                return Ok(false);
            }
            // A targeted acquisition error already NAKed the delivery back.
            LeaseAcquisition::Settled => return Ok(false),
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
    ) -> Result<LeaseHolderCheck> {
        let assessment = if self.activation_faults.as_ref().is_some_and(|faults| {
            faults
                .fail_targeted_lease_holder_read
                .load(std::sync::atomic::Ordering::SeqCst)
        }) {
            Err(anyhow::anyhow!("injected lease-holder lookup failure"))
        } else {
            crate::nats_lease::assess_lease(
                &self.jetstream,
                &self.lease_bucket,
                &self.lease.key_for_session(&activation.session_id),
                &self.lease,
                crate::nats_lease::AcquisitionPolicy::WorkerActivation,
            )
            .await
            .with_context(|| {
                format!(
                    "Failed to read NATS lease for session '{}'",
                    activation.session_id
                )
            })
        };
        match assessment {
            Ok(LeaseAssessment::NoHolder) => Ok(LeaseHolderCheck::Free),
            // Preflight is not ownership: it only permits a claim attempt,
            // whose own create CAS re-observes the key and re-applies the
            // same policy before any purge.
            Ok(LeaseAssessment::Reclaimable { record, reason, .. }) => {
                log_reclaimable_preflight(&activation.session_id, &record, &reason);
                Ok(LeaseHolderCheck::Free)
            }
            Ok(LeaseAssessment::Held { record, age }) => {
                log_held_preflight(&activation.session_id, &record, &age);
                metrics::counter!(harnx_metrics::ACTIVATION_CLAIMS_TOTAL, "outcome" => "busy_foreign")
                    .increment(1);
                Ok(LeaseHolderCheck::Held)
            }
            // A malformed Put is a preflight error, never an absence. It
            // must use the same settlement as an observation failure.
            Ok(LeaseAssessment::MalformedPut) | Err(_) => {
                let error = assessment.err().unwrap_or_else(|| {
                    anyhow::anyhow!(
                        "Failed to decode NATS lease record for session '{}'",
                        activation.session_id
                    )
                });
                log::warn!(
                    "activation lease-holder preflight failed for '{}': {error:#}",
                    activation.session_id
                );
                if self.uses_targeted_activation() {
                    Self::delayed_nak(delivery, super::ActivationNakReason::PreflightNotReady)
                        .await?;
                    // The delivery is already handed back; end the claim here
                    // instead of continuing with a broker-released delivery.
                    Ok(LeaseHolderCheck::Settled)
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
    ) -> Result<PreflightOutcome> {
        if !self
            .targeted_route_preflight_passes(delivery, activation)
            .await?
        {
            return Ok(PreflightOutcome::Finished);
        }
        // This cached KV read avoids metadata and full-log reads while another
        // owner is live. Lease acquisition below remains the ownership CAS.
        match self
            .lease_holder_preflight_passes(delivery, activation)
            .await?
        {
            LeaseHolderCheck::Held => return Ok(PreflightOutcome::Busy),
            LeaseHolderCheck::Settled => return Ok(PreflightOutcome::Finished),
            LeaseHolderCheck::Free => {}
        }
        if !self
            .metadata_preflight_passes(delivery, activation, generation)
            .await?
        {
            return Ok(PreflightOutcome::Finished);
        }

        if self.shutdown.is_cancelled() {
            Self::shutdown_nak(delivery).await?;
            return Ok(PreflightOutcome::Finished);
        }

        if !self
            .wind_up_preflight(delivery, activation, generation)
            .await?
        {
            return Ok(PreflightOutcome::Finished);
        }

        if self
            .activation_status_preflight_finished(delivery, activation)
            .await?
        {
            return Ok(PreflightOutcome::Finished);
        }

        Ok(PreflightOutcome::Ready)
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

/// Diagnostics for a preflight that frees the delivery for a claim attempt,
/// per reclaim reason. Age alone is never proof that the holder process
/// died, and its log must not say so.
fn log_reclaimable_preflight(
    session_id: &str,
    record: &crate::nats_lease::LeaseRecord,
    reason: &ReclaimReason,
) {
    let holder_pid = record
        .holder
        .as_ref()
        .map(|identity| identity.pid)
        .unwrap_or_default();
    match reason {
        ReclaimReason::DeadHolder => log::info!(
            "stale lease holder is dead on this host; proceeding to reclaim: session_id={session_id} holder_worker_id={} holder_pid={holder_pid} holder_generation={}",
            record.worker_id,
            record.generation
        ),
        ReclaimReason::BrokerAge { age, effective_ttl } => log::info!(
            "session lease is old on the broker clock; proceeding to claim reclaim attempt: session_id={session_id} holder_worker_id={} holder_pid={holder_pid} holder_generation={} age_secs={} effective_ttl_secs={}",
            record.worker_id,
            record.generation,
            age.as_secs(),
            effective_ttl.as_secs()
        ),
    }
}

/// Diagnostics for a preflight that keeps deferring to the holder. An
/// unusable clock sample keeps the holder protected and parks the delivery
/// on the existing waiter/recheck path, so it says so loudly.
fn log_held_preflight(
    session_id: &str,
    record: &crate::nats_lease::LeaseRecord,
    age: &crate::nats_lease::HeldAge,
) {
    if matches!(age, crate::nats_lease::HeldAge::UnknownClock) {
        log::warn!(
            "broker time unavailable for lease age check; parking busy activation: session_id={session_id} holder_worker_id={} holder_generation={}",
            record.worker_id,
            record.generation
        );
    } else {
        log::debug!(
            "deferring busy activation before session preflight: session_id={session_id} holder_worker_id={} holder_generation={}",
            record.worker_id,
            record.generation
        );
    }
}
