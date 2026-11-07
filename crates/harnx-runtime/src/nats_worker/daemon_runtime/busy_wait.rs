//! Parking busy activations on this worker instead of handing them back to
//! JetStream.
//!
//! A busy activation used to be NAKed with a delay, so every session that was
//! busy re-polled through the broker every ten-odd seconds — burning delivery
//! budget, admission permits and preflight reads, and adding the NAK delay to
//! every wake. A parked waiter keeps its delivery's progress heartbeats going
//! (JetStream holds the delivery, `info.delivered` stays frozen) and waits for
//! the session to actually free up, then re-runs the normal claim path.
//!
//! Waiters never hold an admission permit: `handle_activation` returns as soon
//! as the delivery is parked, and the waiter is a detached task registered
//! here. Ownership is explicit — a registered waiter always settles its
//! delivery (claim, NAK, ack, term or shutdown NAK) before it deregisters.
//! One exception: a waiter aborted by the shutdown settle timeout never
//! reaches its own disposition, so JetStream recovers its delivery by
//! ack-wait expiry and shutdown's reconciliation clears its slot.
//!
//! Registry shape: per-session slots hold only the release channel and the
//! parked count, so a slot is removed the instant its last parked waiter
//! deregisters. Task handles live in a worker-level list, because a waiter
//! can never observe its own handle finish; that list is pruned on
//! registration and fully drained at shutdown.

use super::*;
use async_nats::jetstream::kv;
use futures_util::StreamExt;

/// Multiple of the per-session waiter capacity allowed across all sessions.
/// Compile-time linked to the admission concurrency in `daemon.rs`
/// (`MAX_CONCURRENT_ADMISSIONS`): a worker pulling at most that many
/// activations at once cannot park more distinct busy sessions than this
/// multiple of a session's own queue without pathological fan-in.
const BUSY_WAITER_TOTAL_CAPACITY_MULTIPLE: usize = super::super::daemon::MAX_CONCURRENT_ADMISSIONS;
/// How long shutdown waits for each waiter to settle its delivery. All
/// waiters settle concurrently under one shared budget: a shutdown NAK and a
/// flush, and the delivery redelivers by ack-wait expiry even if this times
/// out.
const BUSY_WAITER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// One session's parked waiters: the release notification waiters sleep on
/// and how many are parked. Task handles live in the worker-level list, not
/// here, so slot lifetime is bounded by parked waiters alone.
pub(in crate::nats_worker) struct BusyWaiterSlot {
    release: watch::Sender<u64>,
    parked: usize,
}

/// Whether a busy delivery may park on this worker.
enum BusyWaiterAdmission {
    Parked(watch::Receiver<u64>),
    /// The per-session or total waiter capacity is exhausted; the caller must
    /// fall back to a delayed NAK and leave the delivery to JetStream.
    Overflow,
}

/// One busy delivery moving into its parked waiter task: everything the
/// waiter owns from the moment it is spawned.
struct ParkedActivation {
    activation: SessionActivate,
    delivery: ActivationDelivery,
    release_rx: watch::Receiver<u64>,
    shutdown: tokio_util::sync::CancellationToken,
}

/// The mutable state one busy-waiter loop pass carries between its steps, so
/// the step helpers avoid wide signatures.
struct BusyWaiterRun {
    activation: SessionActivate,
    delivery: ActivationDelivery,
    release_rx: watch::Receiver<u64>,
    shutdown: tokio_util::sync::CancellationToken,
    started: std::time::Instant,
    recheck_interval: Duration,
    lease_key: String,
    lease_watch: Option<kv::Watch>,
}

/// What one top-of-loop pass of a busy waiter decided.
enum BusyWaiterStep {
    /// The waiter claimed the freed session and handed the delivery to its
    /// session task.
    Launched,
    /// The delivery is settled (claimed, NAKed, acknowledged or terminated);
    /// the waiter deregisters and exits.
    Settled,
    /// Still busy: the run is returned for the caller to sleep until the next
    /// wake signal.
    StillBusy(Box<BusyWaiterRun>),
}

/// What one item from a busy waiter's lease-key watch means for its sleep.
enum LeaseWatchEvent {
    /// The key was deleted or purged: the holder is gone, claim now.
    Released,
    /// A renewal Put: the key still exists, keep waiting.
    Renewal,
    /// The watch failed or ended; fall back to rechecks and recreate it.
    Broken,
}

fn classify_lease_watch_event<E>(entry: Option<Result<kv::Entry, E>>) -> LeaseWatchEvent {
    match entry {
        Some(Ok(entry))
            if matches!(
                entry.operation,
                kv::Operation::Delete | kv::Operation::Purge
            ) =>
        {
            LeaseWatchEvent::Released
        }
        Some(Ok(_)) => LeaseWatchEvent::Renewal,
        Some(Err(_)) | None => LeaseWatchEvent::Broken,
    }
}

impl WorkerRuntime {
    /// Called whenever this worker stops tracking a session claim. Waiters
    /// parked for that session wake and retry their claim; a miss (no waiter
    /// parked) costs nothing.
    pub(super) async fn notify_session_released(&self, session_id: &str) {
        let waiters = self.busy_waiters.lock().await;
        if let Some(slot) = waiters.get(session_id) {
            let released = slot.release.borrow().wrapping_add(1);
            slot.release.send_replace(released);
        }
    }

    /// Reserve a waiter slot for one busy delivery, if capacity allows.
    async fn register_busy_waiter(&self, session_id: &str) -> BusyWaiterAdmission {
        let mut waiters = self.busy_waiters.lock().await;
        let total: usize = waiters.values().map(|slot| slot.parked).sum();
        if total
            >= self
                .busy_waiter_capacity
                .saturating_mul(BUSY_WAITER_TOTAL_CAPACITY_MULTIPLE)
        {
            return BusyWaiterAdmission::Overflow;
        }
        let receiver = {
            let slot = waiters
                .entry(session_id.to_string())
                .or_insert_with(|| BusyWaiterSlot {
                    release: watch::Sender::new(0),
                    parked: 0,
                });
            if slot.parked >= self.busy_waiter_capacity {
                return BusyWaiterAdmission::Overflow;
            }
            slot.parked += 1;
            slot.release.subscribe()
        };
        record_busy_waiter_gauge(&waiters);
        self.update_busy_waiter_probe(&waiters);
        BusyWaiterAdmission::Parked(receiver)
    }

    async fn finish_busy_waiter(&self, session_id: &str) {
        let mut waiters = self.busy_waiters.lock().await;
        if let Some(slot) = waiters.get_mut(session_id) {
            slot.parked = slot.parked.saturating_sub(1);
            if slot.parked == 0 {
                waiters.remove(session_id);
            }
        }
        record_busy_waiter_gauge(&waiters);
        self.update_busy_waiter_probe(&waiters);
    }

    /// Publish the registry size to the test-only probe, when one is wired.
    fn update_busy_waiter_probe(&self, waiters: &HashMap<String, BusyWaiterSlot>) {
        if let Some(probe) = &self.busy_waiter_probe {
            probe.slots.store(waiters.len(), Ordering::SeqCst);
            probe.parked.store(
                waiters.values().map(|slot| slot.parked).sum(),
                Ordering::SeqCst,
            );
        }
    }

    /// Park one busy delivery on a waiter task, or NAK it back to JetStream
    /// when this worker's waiter capacity is exhausted. Returns once the
    /// delivery is owned by the waiter (or settled by the NAK).
    pub(super) async fn park_busy_activation(
        self: &Arc<Self>,
        activation: SessionActivate,
        mut delivery: ActivationDelivery,
    ) -> Result<()> {
        if !delivery.heartbeat_alive() {
            // The heartbeat stops on its own when an attached lease is lost
            // (a wind-up lease, mid-claim). A dead heartbeat cannot hold the
            // delivery while a waiter sleeps on it: JetStream would redeliver
            // it behind the waiter's back. Hand it back right away instead.
            return Self::busy_nak(&mut delivery).await;
        }
        match self.register_busy_waiter(&activation.session_id).await {
            BusyWaiterAdmission::Parked(release_rx) => {
                tracing::info!(event = "activation_busy_parked",
                    session_id = %activation.session_id,
                    activation_id = %activation.epoch,
                    cluster = %self.cluster,
                    delivery_attempt = delivery.message().info().map_or(0, |info| info.delivered),
                    reason = "busy_parked", "activation parked while session is busy");
                let parked = ParkedActivation {
                    activation,
                    delivery,
                    release_rx,
                    shutdown: self.shutdown.child_token(),
                };
                let worker = Arc::clone(self);
                let handle = tokio::spawn(async move {
                    worker.run_busy_waiter(parked).await;
                });
                let mut handles = self.busy_waiter_handles.lock().await;
                // Prune first: a waiter that already settled frees its slot's
                // bookkeeping here, keeping the list bounded by live tasks.
                handles.retain(|handle| !handle.is_finished());
                handles.push(handle);
                Ok(())
            }
            BusyWaiterAdmission::Overflow => {
                metrics::counter!(harnx_metrics::ACTIVATION_CLAIMS_TOTAL, "outcome" => "busy_overflow")
                    .increment(1);
                Self::busy_nak(&mut delivery).await
            }
        }
    }

    /// Wait until this activation's session is free, then claim it through the
    /// normal path. Wakes on this worker's own release notification (a
    /// same-worker turn finished, or an admission claim that gave up its
    /// reservation without a lease), on the lease key disappearing from KV
    /// (foreign release, TTL expiry after a crash), or on the recheck timer (a
    /// watch dropped by a reconnect, or a missed signal) — whichever comes
    /// first. The waiter also claims once immediately after parking, so a
    /// release that lands between the handler's busy check and registration
    /// cannot cost a full recheck interval. Every wake re-runs the whole claim
    /// path, so live-holder exclusion and stale-holder fencing stay exactly as
    /// they are for a fresh delivery.
    async fn run_busy_waiter(self: &Arc<Self>, parked: ParkedActivation) {
        let ParkedActivation {
            activation,
            delivery,
            release_rx,
            shutdown,
        } = parked;
        let session_id = activation.session_id.clone();
        let mut run = BusyWaiterRun {
            activation,
            delivery,
            release_rx,
            shutdown,
            started: std::time::Instant::now(),
            recheck_interval: self.busy_wait_recheck_interval(),
            lease_key: self.lease.key_for_session(&session_id),
            lease_watch: None,
        };
        while let BusyWaiterStep::StillBusy(mut awake) = self.busy_waiter_iteration(run).await {
            // A wake means a new top-of-loop pass; a shutdown-settled sleep
            // ends the waiter.
            if !self.wait_for_busy_wake(&mut awake).await {
                break;
            }
            run = *awake;
        }
        self.finish_busy_waiter(&session_id).await;
    }

    /// One top-of-loop pass: settle on shutdown or busy-wait-budget expiry,
    /// otherwise run one claim. `StillBusy` hands the run back for the caller
    /// to sleep on; every other outcome has settled (or launched) the delivery.
    async fn busy_waiter_iteration(self: &Arc<Self>, mut run: BusyWaiterRun) -> BusyWaiterStep {
        if run.shutdown.is_cancelled() {
            self.settle_busy_waiter_shutdown(&mut run.delivery, run.started)
                .await;
            return BusyWaiterStep::Settled;
        }
        if run.started.elapsed() >= self.busy_wait_max {
            self.hand_back_expired_busy_waiter(
                &mut run.delivery,
                &run.activation.session_id,
                run.started,
            )
            .await;
            return BusyWaiterStep::Settled;
        }
        self.ensure_busy_lease_watch(
            &mut run.lease_watch,
            &run.lease_key,
            &run.activation.session_id,
        )
        .await;
        self.claim_parked_delivery(run).await
    }

    /// One claim attempt for a parked delivery: launch the claimed session,
    /// settle a claim that ended the delivery, and hand the run back to sleep
    /// on when the session is still busy.
    async fn claim_parked_delivery(self: &Arc<Self>, run: BusyWaiterRun) -> BusyWaiterStep {
        let BusyWaiterRun {
            activation,
            mut delivery,
            release_rx,
            shutdown,
            started,
            recheck_interval,
            lease_key,
            lease_watch,
        } = run;
        let session_id = activation.session_id.clone();

        match self
            .claim_activation(&mut delivery, ClaimOrigin::BusyWaiter)
            .await
        {
            Ok(ClaimOutcome::Claimed(claimed)) => {
                harnx_metrics::record_activation_phase("busy_wait", started.elapsed());
                if let Err(error) = self.launch_claimed_session(claimed, delivery).await {
                    log::warn!(
                        "busy waiter failed to launch claimed session '{session_id}': {error:#}"
                    );
                }
                BusyWaiterStep::Launched
            }
            // Still busy (another waiter claimed first, or the next turn
            // already started): go back to sleep on the same delivery.
            Ok(ClaimOutcome::Busy(_)) => {
                if !delivery.heartbeat_alive() {
                    // A claim-time lease loss (wind-up) stopped the
                    // heartbeat, so the waiter can no longer hold this
                    // delivery: hand it back instead of sleeping until
                    // JetStream redelivers it behind the waiter's back.
                    harnx_metrics::record_activation_phase("busy_wait", started.elapsed());
                    let _ = Self::busy_nak(&mut delivery).await;
                    return BusyWaiterStep::Settled;
                }
                BusyWaiterStep::StillBusy(Box::new(BusyWaiterRun {
                    activation,
                    delivery,
                    release_rx,
                    shutdown,
                    started,
                    recheck_interval,
                    lease_key,
                    lease_watch,
                }))
            }
            Ok(ClaimOutcome::Settled) => {
                harnx_metrics::record_activation_phase("busy_wait", started.elapsed());
                let _ = delivery.stop_heartbeat().await;
                BusyWaiterStep::Settled
            }
            Err(error) => {
                harnx_metrics::record_activation_phase("busy_wait", started.elapsed());
                let _ = Self::settle_busy_claim_error(&mut delivery, &shutdown, error).await;
                BusyWaiterStep::Settled
            }
        }
    }

    /// Hand a parked delivery back un-delayed on shutdown: another worker can
    /// pick it up right away.
    async fn settle_busy_waiter_shutdown(
        &self,
        delivery: &mut ActivationDelivery,
        started: std::time::Instant,
    ) {
        harnx_metrics::record_activation_phase("busy_wait", started.elapsed());
        let _ = Self::shutdown_nak(delivery).await;
        self.flush_shutdown_disposition().await;
    }

    /// A session busy for this long is stuck, not slow: hand the activation
    /// back so JetStream's delivery accounting decides its fate instead of
    /// this worker holding it forever.
    async fn hand_back_expired_busy_waiter(
        &self,
        delivery: &mut ActivationDelivery,
        session_id: &str,
        started: std::time::Instant,
    ) {
        harnx_metrics::record_activation_phase("busy_wait", started.elapsed());
        if let Err(error) = Self::busy_nak(delivery).await {
            log::warn!(
                "failed to hand busy-wait-budget-expired activation for '{session_id}' back to JetStream: {error:#}"
            );
        }
    }

    /// Start watching the session's lease key when no watch is live. Best
    /// effort: a broken watch only costs recheck latency, but say so instead
    /// of hiding it.
    async fn ensure_busy_lease_watch(
        &self,
        lease_watch: &mut Option<kv::Watch>,
        lease_key: &str,
        session_id: &str,
    ) {
        if lease_watch.is_some() || self.lease_watch_faulted() {
            return;
        }
        *lease_watch = match self.lease_bucket.watch(lease_key).await {
            Ok(watch) => Some(watch),
            Err(error) => {
                log::debug!(
                    "busy waiter lease watch failed to start for '{session_id}'; falling back to rechecks: {error}"
                );
                None
            }
        };
    }

    /// Count a failed parked-claim like admission does, then hand the delivery
    /// back: un-delayed on shutdown (another worker picks it up right away),
    /// delayed otherwise.
    async fn settle_busy_claim_error(
        delivery: &mut ActivationDelivery,
        shutdown: &tokio_util::sync::CancellationToken,
        error: anyhow::Error,
    ) -> Result<()> {
        metrics::counter!(harnx_metrics::ACTIVATION_CLAIMS_TOTAL, "outcome" => "error")
            .increment(1);
        log::warn!(
            "busy waiter claim failed for '{}': {error:#}",
            delivery.session_id().as_deref().unwrap_or("unknown")
        );
        if shutdown.is_cancelled() {
            Self::shutdown_nak(delivery).await
        } else {
            Self::delayed_nak(delivery, ActivationNakReason::ClaimError).await
        }
    }

    /// Never re-check faster than the lease TTL: a holder that stops renewing
    /// loses the key within the TTL, so a faster loop would just spin claim
    /// attempts against a live holder.
    fn busy_wait_recheck_interval(&self) -> Duration {
        Duration::max(self.lease.ttl, Duration::from_secs(1))
    }

    /// Sleep until a real wake: this worker's release notification, a
    /// lease-key Delete/Purge, or the recheck deadline. The deadline is
    /// absolute across the whole sleep, and renewal Puts — the most common
    /// event while a holder is alive — are ignored without resetting it, so a
    /// long-held session costs no claim attempts. The busy-wait budget also
    /// caps the sleep, so the waiter hands the delivery back at expiry instead
    /// of holding it one lease TTL longer.
    ///
    /// Returns `false` when shutdown settled the delivery mid-sleep.
    async fn wait_for_busy_wake(self: &Arc<Self>, run: &mut BusyWaiterRun) -> bool {
        let deadline = (tokio::time::Instant::now() + run.recheck_interval).min(
            tokio::time::Instant::from_std(run.started + self.busy_wait_max),
        );
        // The watch stays bound across loop iterations: a renewal Put only
        // re-polls, and every exit hands the watch back — or discards it when
        // it is genuinely broken — so the next pass never silently loses
        // foreign-release events behind a dropped binding.
        let mut lease_watch = run.lease_watch.take();
        loop {
            let recheck = tokio::time::sleep_until(deadline);
            tokio::pin!(recheck);
            tokio::select! {
                biased;
                _ = run.shutdown.cancelled() => {
                    run.lease_watch = lease_watch;
                    self.settle_busy_waiter_shutdown(&mut run.delivery, run.started)
                        .await;
                    return false;
                }
                _ = run.release_rx.changed() => {
                    run.lease_watch = lease_watch;
                    return true;
                }
                entry = async {
                    match lease_watch.as_mut() {
                        Some(watch) => watch.next().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match classify_lease_watch_event(entry) {
                        // Renewal Puts keep the key alive, so keep waiting on
                        // the same absolute deadline — and keep the watch.
                        LeaseWatchEvent::Renewal => continue,
                        LeaseWatchEvent::Released => {
                            run.lease_watch = lease_watch;
                            return true;
                        }
                        LeaseWatchEvent::Broken => {
                            log::debug!(
                                "busy waiter lease watch failed for '{}'; falling back to rechecks",
                                run.activation.session_id
                            );
                            run.lease_watch = None;
                            return true;
                        }
                    }
                }
                _ = &mut recheck => {
                    run.lease_watch = lease_watch;
                    return true;
                }
            }
        }
    }

    /// Test seam: pretend the lease-key watch cannot be created, forcing
    /// recovery through the recheck timer alone.
    fn lease_watch_faulted(&self) -> bool {
        self.activation_faults
            .as_ref()
            .is_some_and(|faults| faults.drop_lease_watch.load(Ordering::SeqCst))
    }

    /// Settle every parked waiter during shutdown. Waiters NAK their
    /// deliveries back un-delayed so another worker picks them up right
    /// away; all of them settle concurrently under one shared budget, so a
    /// rolling deploy never strands a parked activation behind a dying
    /// client and shutdown leaves time for the active-session drain.
    pub(in crate::nats_worker) async fn settle_busy_waiters(&self) {
        let handles = {
            let mut handles = self.busy_waiter_handles.lock().await;
            std::mem::take(&mut *handles)
        };
        let settled = handles
            .into_iter()
            .map(|mut handle| async move {
                match tokio::time::timeout(BUSY_WAITER_SHUTDOWN_TIMEOUT, &mut handle).await {
                    Ok(Ok(())) => {}
                    Ok(Err(join_error)) => {
                        log::warn!("busy waiter failed during shutdown: {join_error}");
                    }
                    Err(_) => {
                        // Dropping the JoinHandle would detach the waiter,
                        // and it keeps this runtime and its delivery's
                        // heartbeat alive after shutdown reports success.
                        // Abort and join instead: an unsettled delivery
                        // redelivers by ack-wait expiry, the same backstop
                        // as an outer shutdown-deadline overrun.
                        log::warn!("timed out settling busy waiter during shutdown; aborting it");
                        handle.abort();
                        if let Err(join_error) = handle.await {
                            log::warn!("failed to join aborted busy waiter: {join_error}");
                        }
                    }
                }
            })
            .collect::<Vec<_>>();
        futures_util::future::join_all(settled).await;
        // Every drained handle has now joined, been aborted and joined, or
        // failed — no waiter from this list can still decrement its own slot.
        // Admission is closed (the shutdown token is cancelled, and a claim
        // settles a delivery before it could park one), so a slot still
        // counted belongs to a waiter aborted mid-settlement: reconcile the
        // registry to empty so the gauge and probe report zero parked waiters
        // instead of a phantom the aborted task can never clear. The one
        // in-flight race — a delivery that parked between the handle drain
        // and here — settles itself against the cancelled token and
        // deregisters on its own, and `finish_busy_waiter` saturates.
        let mut waiters = self.busy_waiters.lock().await;
        waiters.clear();
        record_busy_waiter_gauge(&waiters);
        self.update_busy_waiter_probe(&waiters);
    }
}

fn record_busy_waiter_gauge(waiters: &HashMap<String, BusyWaiterSlot>) {
    let total: usize = waiters.values().map(|slot| slot.parked).sum();
    metrics::gauge!(harnx_metrics::WORKER_BUSY_WAITERS).set(total as f64);
}
