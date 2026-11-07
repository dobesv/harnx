//! Per-worker runtime state and `SessionActivate` handling: lease
//! acquisition, this session's tool-server refcount, control-plane
//! subscription, and handing the claimed session off to execution.

mod activation_preflight;
mod busy_wait;

use super::activation_delivery::{ActivationDelivery, ActivationNakReason};
use super::activation_failure::ActivationFailureTracker;
use super::backend::NatsSessionLogBackend;
use super::control::{control_subject, SessionControlHandler};
use super::daemon::{SessionActivate, SessionActivationRoute, WorkerActivationMode};
use super::daemon_background::BackgroundServices;
use super::execution_control::{FinishCause, WorkerExecution};
use super::server_reconciler::{tool_servers_for_activation, ServerReconciler};
use crate::config::{ConfigLock, GlobalConfig};
use crate::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
use crate::nats_metrics;
use anyhow::{Context, Result};
use async_nats::jetstream;
use rand::RngExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;

/// Longest `handle_activation` waits for this session's own tool servers to
/// start before giving up and continuing anyway.
///
/// Deliberately well under `WORK_NOTIFY_ACK_WAIT`: tool-server starts now run
/// concurrently (see `ServerReconciler::session_started`), so this only needs
/// to cover one server's own startup timeout plus margin, not the sum of
/// several. Comfortably clearing this margin matters because JetStream
/// redelivers an unacked activation once its ack wait runs out, and each
/// redelivery re-acquires the lease and fences the still-running previous
/// attempt.
pub(super) const SESSION_TOOL_SERVER_START_TIMEOUT: Duration = Duration::from_secs(20);
const BUSY_NAK_BASE_DELAY: Duration = Duration::from_secs(10);
const BUSY_NAK_MAX_JITTER: Duration = Duration::from_secs(2);
/// Deliveries after which a worker terminates an activation instead of handing
/// it back for another one. Busy deferrals don't trigger this worker limit:
/// an activation whose session is busy parks on a busy waiter (`busy_wait.rs`)
/// without advancing its current JetStream delivery count. Busy NAKs still
/// increment that shared count, so a later non-busy failure can hit this limit
/// after earlier busy redeliveries. Busy-only broker cycles remain subject to
/// the consumer's `max_deliver` backstop. Failures after
/// the lease is claimed are recorded on the session by the failure budget long
/// before; failures before it is claimed, and any while the failure counter is
/// unavailable, aren't counted and end here without an `Error`. Kept below the
/// consumer's `max_deliver`, so a worker sees the activation and says why it
/// gave up before JetStream stops delivering it silently.
pub const MAX_ACTIVATION_DELIVERIES: i64 = 100;

/// Borrowed parameters for [`WorkerRuntime::spawn_control_listener`].
pub(super) struct ControlListenerCtx<'a> {
    pub(super) client: &'a async_nats::Client,
    pub(super) jetstream: &'a jetstream::Context,
    pub(super) session_id: &'a str,
    pub(super) lease: &'a Arc<NatsSessionLease>,
    pub(super) backend: &'a NatsSessionLogBackend,
    pub(super) abort_signal: &'a crate::utils::AbortSignal,
}

struct PreparedControl {
    task: JoinHandle<()>,
    hitl_decision_rx: tokio::sync::mpsc::UnboundedReceiver<super::control::AppliedHitlDecision>,
}

/// Borrowed parameters for [`WorkerRuntime::prepare_activation_control`].
struct ActivationControlCtx<'a> {
    activation: &'a SessionActivate,
    lease: &'a Arc<NatsSessionLease>,
    abort_signal: &'a crate::utils::AbortSignal,
}

struct ClaimedActivation {
    activation: SessionActivate,
    lease: Arc<NatsSessionLease>,
    reservation_generation: u64,
    span: tracing::Span,
    lease_acquired_at: std::time::Instant,
}

struct PreparedActivation {
    activation: SessionActivate,
    lease: Arc<NatsSessionLease>,
    abort_signal: crate::utils::AbortSignal,
    control: PreparedControl,
    execution: WorkerExecution,
    delivery: ActivationDelivery,
    shutdown: tokio_util::sync::CancellationToken,
    span: tracing::Span,
    lease_acquired_at: std::time::Instant,
}

pub(super) struct ActiveSession {
    reservation_generation: u64,
    shutdown: tokio_util::sync::CancellationToken,
    join: JoinHandle<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivationReservationState {
    Starting,
    Running,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ActivationReservation {
    generation: u64,
    stream_sequence: Option<u64>,
    state: ActivationReservationState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReservationOutcome {
    Reserved(u64),
    SameDelivery,
    DistinctDelivery,
}

/// Outcome of one whole claim attempt on a delivery, settled in one place
/// (see issue #2206): the claim succeeded, the session is busy and the caller
/// must park the delivery, or the delivery is already settled (acknowledged,
/// NAKed, terminated, or handed back untouched) and needs nothing further.
enum ClaimOutcome {
    Claimed(ClaimedActivation),
    /// Carries the activation so a busy waiter can retry with it.
    Busy(SessionActivate),
    Settled,
}

/// Outcome of claiming an already-reserved activation. `Busy` means the
/// reservation has been released and the caller owns deciding what happens to
/// the delivery: park it, or NAK it back.
enum ClaimedOrBusy {
    Claimed(ClaimedActivation),
    Busy(SessionActivate),
    Settled,
}

/// Who is running a claim. An admission claim that ends without a lease wakes
/// parked waiters once it releases its reservation; a busy waiter's own claim
/// releases quietly, or every waiter would wake itself into a claim loop
/// against a live holder.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ClaimOrigin {
    Admission,
    BusyWaiter,
}

/// The reservation a claim runs under, and who is claiming.
#[derive(Clone, Copy)]
struct ClaimContext {
    reservation_generation: u64,
    origin: ClaimOrigin,
}

/// Outcome of the preflight chain. `Busy` leaves the delivery untouched: the
/// session is held elsewhere and the caller parks or NAKs. `Finished` means
/// the delivery has already been settled (acknowledged, NAKed or
/// terminated) by one of the preflights, or it settled as a duplicate.
pub(super) enum PreflightOutcome {
    Ready,
    Busy,
    Finished,
}

/// Whether the session lease key is held by someone else. `Settled` means a
/// targeted lookup error already NAKed the delivery back, so the claim must
/// end there instead of continuing with a broker-released delivery.
#[derive(PartialEq, Eq, Clone, Copy)]
pub(super) enum LeaseHolderCheck {
    Free,
    Held,
    Settled,
}

/// Outcome of attempting the activation lease. `Held` leaves the delivery
/// untouched for the caller to decide; `Settled` means a targeted
/// acquisition error already NAKed the delivery back.
enum LeaseAcquisition {
    Acquired(Arc<NatsSessionLease>),
    Held,
    Settled,
}

fn record_publish_to_delivery(
    delivered: time::OffsetDateTime,
    published: time::OffsetDateTime,
) -> Duration {
    // Broker and worker clocks can disagree; retain the delivery sample at zero.
    let elapsed = Duration::try_from(delivered - published).unwrap_or_default();
    harnx_metrics::record_activation_phase("publish_to_delivery", elapsed);
    elapsed
}

/// How a durably recorded failure is reported when its activation is
/// terminated: a refusal, or a failure that spent its budget or ended a turn.
fn durable_termination_reason(error: &anyhow::Error) -> &'static str {
    if super::daemon_session_exec::is_activation_refusal(error) {
        "refused"
    } else {
        "durably failed"
    }
}

fn busy_nak_delay(jitter: Duration) -> Duration {
    BUSY_NAK_BASE_DELAY + jitter.min(BUSY_NAK_MAX_JITTER)
}

fn agent_activation_span(
    headers: Option<&async_nats::HeaderMap>,
    session_id: &str,
) -> tracing::Span {
    let parent_cx = headers
        .map(harnx_telemetry::propagate::extract_context_from_nats)
        .unwrap_or_default();
    let span = tracing::info_span!(
        "agent_activation",
        otel.kind = "consumer",
        harnx.session.id = session_id,
    );
    harnx_telemetry::set_span_parent(&span, parent_cx);
    span
}

pub(super) struct WorkerRuntime {
    pub(super) config: GlobalConfig,
    pub(super) instance_id: harnx_core::instance::ServerScope,
    pub(super) _remote_cleanup: Option<AbortOnDropHandle<()>>,
    pub(super) _background_services: Arc<Mutex<Option<BackgroundServices>>>,
    pub(super) background_services_attempted: tokio::sync::watch::Receiver<bool>,
    /// `None` for a consuming worker, or a managing worker with nothing
    /// configured to spawn anywhere.
    pub(super) server_reconciler: Option<Arc<ServerReconciler>>,
    pub(super) cluster: String,
    pub(super) activation_route: SessionActivationRoute,
    pub(super) activation_mode: WorkerActivationMode,
    pub(super) activation_heartbeat_interval: Duration,
    pub(super) manage_servers: bool,
    pub(super) worker_id: String,
    pub(super) identity: crate::worker_identity::WorkerReadiness,
    pub(super) lease: NatsLeaseConfig,
    pub(super) lease_bucket: jetstream::kv::Store,
    pub(super) activation_failures: ActivationFailureTracker,
    pub(super) jetstream: jetstream::Context,
    pub(super) session_metadata: crate::nats_session_metadata::SessionMetadataStore,
    /// Shared NATS client for control-plane subscriptions (cloned per session
    /// rather than reconnecting on each activation).
    pub(super) client: async_nats::Client,
    pub(super) call_fn: Option<crate::agent_loop::AgentCallFn>,
    pub(super) generation: AtomicU64,
    pub(super) shutdown: tokio_util::sync::CancellationToken,
    pub(super) active: Mutex<HashMap<String, ActiveSession>>,
    pub(super) reservations: Mutex<HashMap<String, ActivationReservation>>,
    /// Busy deliveries parked on this worker (see `busy_wait.rs`).
    pub(super) busy_waiters: Mutex<HashMap<String, busy_wait::BusyWaiterSlot>>,
    /// Task handles of every spawned busy waiter, parked or completing, for
    /// shutdown settlement. Kept outside the per-session slots so a slot can
    /// be removed the moment its last parked waiter finishes, instead of
    /// staying until its own handle observes completion (which it never can).
    pub(super) busy_waiter_handles: Mutex<Vec<JoinHandle<()>>>,
    pub(super) busy_wait_max: Duration,
    pub(super) busy_waiter_capacity: usize,
    /// Test-only fault seams; `None` in production.
    pub(super) activation_faults: Option<Arc<super::daemon_config::ActivationFaults>>,
    /// Test-only live view of the busy-waiter registry; `None` in production.
    pub(super) busy_waiter_probe: Option<Arc<super::daemon_config::BusyWaiterProbe>>,
}

impl WorkerRuntime {
    fn uses_targeted_activation(&self) -> bool {
        self.activation_mode == WorkerActivationMode::WorkerTargeted
    }

    async fn reserve_activation(
        &self,
        session_id: &str,
        stream_sequence: Option<u64>,
    ) -> ReservationOutcome {
        let mut reservations = self.reservations.lock().await;
        if let Some(reservation) = reservations.get(session_id) {
            return if stream_sequence.is_some() && stream_sequence == reservation.stream_sequence {
                ReservationOutcome::SameDelivery
            } else {
                ReservationOutcome::DistinctDelivery
            };
        }
        let generation = self.generation.fetch_add(1, Ordering::SeqCst);
        reservations.insert(
            session_id.to_string(),
            ActivationReservation {
                generation,
                stream_sequence,
                state: ActivationReservationState::Starting,
            },
        );
        ReservationOutcome::Reserved(generation)
    }

    async fn mark_activation_running(&self, session_id: &str, generation: u64) -> bool {
        let mut reservations = self.reservations.lock().await;
        let Some(reservation) = reservations.get_mut(session_id) else {
            return false;
        };
        if reservation.generation != generation
            || reservation.state != ActivationReservationState::Starting
        {
            return false;
        }
        reservation.state = ActivationReservationState::Running;
        true
    }

    async fn release_activation_reservation(&self, session_id: &str, generation: u64) {
        let mut reservations = self.reservations.lock().await;
        if reservations
            .get(session_id)
            .is_some_and(|reservation| reservation.generation == generation)
        {
            reservations.remove(session_id);
        }
    }

    async fn release_activation_tracking(&self, session_id: &str, generation: u64) {
        self.release_activation_reservation(session_id, generation)
            .await;
        let mut active = self.active.lock().await;
        if active
            .get(session_id)
            .is_some_and(|session| session.reservation_generation == generation)
        {
            active.remove(session_id);
        }
        drop(active);
        self.notify_session_released(session_id).await;
    }

    /// Close admission's active-session side and await every single-owner
    /// supervisor. Session supervisors retain their message, lease, and NATS
    /// client until failover cleanup completes.
    pub(super) async fn shutdown_active_sessions(&self) {
        let sessions = {
            let mut active = self.active.lock().await;
            active.drain().collect::<Vec<_>>()
        };
        for (_, session) in &sessions {
            session.shutdown.cancel();
        }
        for (session_id, session) in sessions {
            let generation = session.reservation_generation;
            if let Err(error) = session.join.await {
                log::warn!("worker session supervisor failed during shutdown: {error}");
            }
            self.release_activation_reservation(&session_id, generation)
                .await;
            self.notify_session_released(&session_id).await;
        }
        match tokio::time::timeout(Duration::from_secs(2), self.client.flush()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                log::warn!("failed to flush worker NATS client during shutdown: {error}");
            }
            Err(error) => {
                log::warn!("timed out flushing worker NATS client during shutdown: {error}");
            }
        }
    }

    /// A worker that doesn't manage its own servers, or has nothing
    /// configured to spawn anywhere, has no reconciler and this is a no-op.
    ///
    /// Only the actual server start (`ServerReconciler::start_claimed`) is
    /// bounded by `SESSION_TOOL_SERVER_START_TIMEOUT` and, on timeout, left
    /// running in the background rather than aborted — a session degraded to
    /// fewer tools is far better than an unacked activation that JetStream
    /// redelivers forever. Registering this session as a user
    /// (`ServerReconciler::claim_users`) is awaited directly, unbounded and
    /// un-detached: `end_session_tool_servers` can run as soon as this
    /// activation's ack fails, or when the session ends, and must always see
    /// an accurate registration to release. A registration that instead
    /// landed later, from an abandoned background task, would pin its server
    /// as a "user" that no future `session_ended` call can ever remove.
    pub(super) async fn start_session_tool_servers(&self, activation: &SessionActivate) {
        let Some(reconciler) = self.server_reconciler.clone() else {
            return;
        };
        let metadata = match self.session_metadata.get(&activation.session_id).await {
            Ok(Some(record)) => record.metadata,
            Ok(None) => {
                log::warn!(
                    "refusing tool-server startup for session without metadata: session_id={}",
                    activation.session_id
                );
                return;
            }
            Err(error) => {
                log::warn!(
                    "failed to load session metadata for tool-server startup: session_id={} error={error:#}",
                    activation.session_id
                );
                return;
            }
        };
        let servers = tool_servers_for_activation(&self.config, &metadata);
        if servers.is_empty() {
            return;
        }
        let to_start = reconciler
            .claim_users(&activation.session_id, servers)
            .await;
        Self::wait_for_tool_server_start(reconciler, to_start, &activation.session_id).await;
    }

    pub(super) async fn wait_for_tool_server_start(
        reconciler: Arc<ServerReconciler>,
        to_start: Vec<crate::config::ToolServerConfig>,
        user_id: &str,
    ) {
        if to_start.is_empty() {
            return;
        }
        let server_names = to_start
            .iter()
            .map(|server| server.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let task = tokio::spawn(async move { reconciler.start_claimed(to_start).await });
        match tokio::time::timeout(SESSION_TOOL_SERVER_START_TIMEOUT, task).await {
            Ok(Ok(())) => {}
            Ok(Err(join_error)) => {
                log::warn!(
                    "session '{}' tool-server startup task panicked: {join_error}",
                    user_id
                );
            }
            Err(_) => {
                log::warn!(
                    "session '{}' tool-server startup ({}) exceeded {}s; continuing without waiting further (still starting in the background)",
                    user_id,
                    server_names,
                    SESSION_TOOL_SERVER_START_TIMEOUT.as_secs()
                );
            }
        }
    }

    pub(super) async fn end_session_tool_servers(&self, session_id: &str) {
        if let Some(reconciler) = &self.server_reconciler {
            reconciler.session_ended(session_id).await;
        }
    }

    async fn acquire_activation_lease(
        &self,
        activation: &SessionActivate,
        generation: u64,
    ) -> Result<Option<Arc<NatsSessionLease>>> {
        let lease = NatsSessionLease::acquire_for_execution(
            NatsLeaseAcquireParams {
                jetstream: self.jetstream.clone(),
                session_id: &activation.session_id,
                worker_id: self.worker_id.clone(),
                generation,
                config: self.lease.clone(),
                session_metadata: Some(self.session_metadata.clone()),
            },
            activation.execution_id.clone(),
        )
        .await?;
        Ok(lease.map(Arc::new))
    }

    async fn shutdown_nak(delivery: &mut ActivationDelivery) -> Result<()> {
        delivery.nak(None, ActivationNakReason::Shutdown).await
    }

    async fn delayed_nak(
        delivery: &mut ActivationDelivery,
        reason: ActivationNakReason,
    ) -> Result<()> {
        let exponent = u32::try_from(delivery.delivered().saturating_sub(1))
            .unwrap_or(u32::MAX)
            .min(5);
        let millis = 100_u64.saturating_mul(1_u64 << exponent).min(2_000);
        Self::redeliver_later(delivery, Duration::from_millis(millis), reason).await
    }

    async fn busy_nak(delivery: &mut ActivationDelivery) -> Result<()> {
        let jitter_millis = rand::rng()
            .random_range(0..=u64::try_from(BUSY_NAK_MAX_JITTER.as_millis()).unwrap_or(u64::MAX));
        Self::redeliver_later(
            delivery,
            busy_nak_delay(Duration::from_millis(jitter_millis)),
            ActivationNakReason::Busy,
        )
        .await
    }

    /// Hand the activation back for another delivery after `delay`, unless it
    /// has used up [`MAX_ACTIVATION_DELIVERIES`]. Shutdown hands activations
    /// back through [`Self::shutdown_nak`] instead, so a rolling deploy never
    /// spends an activation's last delivery. Busy activations normally park
    /// on a waiter rather than reach this; a delayed NAK is the fallback for
    /// busy deliveries past the waiter capacity or the busy-wait budget.
    async fn redeliver_later(
        delivery: &mut ActivationDelivery,
        delay: Duration,
        reason: ActivationNakReason,
    ) -> Result<()> {
        if delivery.delivered() < MAX_ACTIVATION_DELIVERIES {
            return delivery.nak(Some(delay), reason).await;
        }
        if reason == ActivationNakReason::Busy {
            // A busy deferral means the session is legitimately occupied, not
            // that anything is failing, so spending the delivery limit on it
            // would drop work a live session still owes (issue #2636). Keep
            // handing it back: the consumer's `max_deliver` backstop (200) ends
            // a delivery nothing ever settles.
            return delivery.nak(Some(delay), reason).await;
        }
        delivery.terminate("delivery limit").await?;
        log::warn!(
            "terminated activation after {} deliveries instead of redelivering it ({}): \
             activation={} session_id={}",
            delivery.delivered(),
            reason.label(),
            delivery.failure_key(),
            delivery.session_id().as_deref().unwrap_or("unknown"),
        );
        metrics::counter!(harnx_metrics::ACTIVATION_CLAIMS_TOTAL, "outcome" => "delivery_limit_term")
            .increment(1);
        Ok(())
    }

    async fn flush_shutdown_disposition(&self) {
        if self.activation_faults.as_ref().is_some_and(|faults| {
            faults
                .stall_shutdown_flush
                .load(std::sync::atomic::Ordering::SeqCst)
        }) {
            // Test seam: hold the shutdown flush open forever, so the
            // shutdown settle timeout has to abort and join this waiter.
            std::future::pending::<()>().await;
        }
        match tokio::time::timeout(Duration::from_secs(2), self.client.flush()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                log::warn!("failed to flush shutdown activation NAK: {error}");
            }
            Err(_) => {
                log::warn!("timed out flushing shutdown activation NAK");
            }
        }
    }

    async fn activation_is_covered(&self, activation: &SessionActivate) -> Result<bool> {
        let backend = NatsSessionLogBackend::new(
            self.jetstream.clone(),
            &activation.session_id,
            self.lease.replicas,
        );
        let entries = backend.load_events_latest_async().await?;
        let requested_seq = activation.requested_seq.or_else(|| {
            entries.iter().rev().find_map(|(seq, entry)| match entry {
                harnx_core::session::SessionLogEntry::Message { role, content, .. }
                    if role.is_user() =>
                {
                    // Skip timing notices — use shared classifier to ensure only valid notices skipped
                    if harnx_core::timing_identity::is_timing_notice(&content.to_text()) {
                        None
                    } else {
                        Some(*seq)
                    }
                }
                _ => None,
            })
        });
        let Some(requested_seq) = requested_seq else {
            return Ok(false);
        };
        Ok(
            crate::nats_session::requested_seq_status(&entries, requested_seq)?
                == crate::nats_session::RequestedSeqStatus::Covered,
        )
    }

    fn validate_targeted_activation(&self, activation: &SessionActivate) -> Result<()> {
        anyhow::ensure!(
            activation.target_worker_id.as_deref() == Some(self.worker_id.as_str()),
            "targeted activation for session '{}' names worker {:?}, but consumer belongs to '{}'",
            activation.session_id,
            activation.target_worker_id,
            self.worker_id
        );
        anyhow::ensure!(
            activation.requested_seq.is_some(),
            "targeted activation for session '{}' is missing requested_seq",
            activation.session_id
        );
        Ok(())
    }

    async fn clear_activation_failures(&self, delivery: &ActivationDelivery) {
        if let Err(error) = delivery
            .clear_failure_count(&self.activation_failures)
            .await
        {
            log::warn!(
                "failed to clear activation failure counter: key={} error={error:#}",
                delivery.failure_key()
            );
        }
    }

    async fn acknowledge_activation(
        &self,
        delivery: &mut ActivationDelivery,
        reason: &str,
    ) -> Result<()> {
        delivery.ack(reason).await?;
        self.clear_activation_failures(delivery).await;
        Ok(())
    }

    async fn terminate_activation(
        &self,
        delivery: &mut ActivationDelivery,
        reason: &str,
    ) -> Result<()> {
        delivery.terminate(reason).await?;
        match reason {
            "durably failed" => {
                metrics::counter!(harnx_metrics::ACTIVATION_CLAIMS_TOTAL, "outcome" => "failure_budget_term").increment(1);
            }
            "refused" => {
                metrics::counter!(harnx_metrics::ACTIVATION_CLAIMS_TOTAL, "outcome" => "refused_term").increment(1);
            }
            _ => {}
        }
        self.clear_activation_failures(delivery).await;
        Ok(())
    }

    async fn decode_activation(
        &self,
        delivery: &mut ActivationDelivery,
    ) -> Result<Option<SessionActivate>> {
        match serde_json::from_slice(&delivery.message().payload) {
            Ok(activation) => Ok(Some(activation)),
            Err(error) => {
                log::warn!("terminating malformed SessionActivate: {error}");
                self.terminate_activation(delivery, "malformed").await?;
                Ok(None)
            }
        }
    }

    async fn activation_status_preflight_finished(
        &self,
        delivery: &mut ActivationDelivery,
        activation: &SessionActivate,
    ) -> Result<bool> {
        match self.activation_is_covered(activation).await {
            Ok(true) => {
                self.acknowledge_activation(delivery, "covered activation")
                    .await?;
                Ok(true)
            }
            Ok(false) => Ok(false),
            Err(error) => {
                log::warn!(
                    "activation status read failed for session '{}': {error:#}",
                    activation.session_id
                );
                Self::delayed_nak(delivery, ActivationNakReason::PreflightNotReady).await?;
                Ok(true)
            }
        }
    }

    async fn acquire_or_defer_activation(
        &self,
        delivery: &mut ActivationDelivery,
        activation: &SessionActivate,
        generation: u64,
    ) -> Result<LeaseAcquisition> {
        let acquired = if self.activation_faults.as_ref().is_some_and(|faults| {
            faults
                .fail_targeted_lease_acquire
                .load(std::sync::atomic::Ordering::SeqCst)
        }) {
            Err(anyhow::anyhow!("injected activation lease failure"))
        } else {
            self.acquire_activation_lease(activation, generation).await
        };
        match acquired {
            Ok(Some(lease)) => Ok(LeaseAcquisition::Acquired(lease)),
            Ok(None) => {
                metrics::counter!(harnx_metrics::ACTIVATION_CLAIMS_TOTAL, "outcome" => "lease_held").increment(1);
                // The caller decides what a held lease means for the
                // delivery: the claim path parks it, the failure-budget path
                // NAKs it back.
                Ok(LeaseAcquisition::Held)
            }
            Err(error) if self.uses_targeted_activation() => {
                log::warn!(
                    "targeted activation lease attempt failed for session '{}': {error:#}",
                    activation.session_id
                );
                // Settle exactly once: the delivery is already handed back,
                // so the caller must end the claim, never park it.
                Self::delayed_nak(delivery, ActivationNakReason::ClaimError).await?;
                Ok(LeaseAcquisition::Settled)
            }
            Err(error) => Err(error),
        }
    }

    async fn prepare_activation_control(
        &self,
        ctx: &ActivationControlCtx<'_>,
    ) -> Result<PreparedControl> {
        let ActivationControlCtx {
            activation,
            lease,
            abort_signal,
        } = *ctx;
        let backend = NatsSessionLogBackend::new(
            self.jetstream.clone(),
            &activation.session_id,
            self.lease.replicas,
        );
        match Self::spawn_control_listener(ControlListenerCtx {
            client: &self.client,
            jetstream: &self.jetstream,
            session_id: &activation.session_id,
            lease,
            backend: &backend,
            abort_signal,
        })
        .await
        {
            Ok(control) => Ok(control),
            Err(error) => Err(error),
        }
    }

    fn log_activation_claim(&self, activation: &SessionActivate, lease: &NatsSessionLease) {
        log::info!(
            "session activate claimed: session_id={} worker_id={} worker_pid={} build={} activation_route={:?} revision={} epoch={}",
            activation.session_id,
            lease.worker_id(),
            self.identity.pid,
            self.identity.build,
            self.activation_route,
            lease.fence_token(),
            activation.epoch
        );
    }

    async fn prepare_claimed_activation(
        self: &Arc<Self>,
        claimed: ClaimedActivation,
        mut delivery: ActivationDelivery,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<PreparedActivation> {
        let ClaimedActivation {
            activation,
            lease,
            reservation_generation: _,
            span,
            lease_acquired_at,
        } = claimed;
        let execution = WorkerExecution::claim(&activation, &lease);
        self.log_activation_claim(&activation, &lease);

        let abort_signal = crate::utils::create_abort_signal();
        let control = match self
            .prepare_activation_control(&ActivationControlCtx {
                activation: &activation,
                lease: &lease,
                abort_signal: &abort_signal,
            })
            .await
        {
            Ok(control) => control,
            Err(error) => {
                let error = if self.shutdown.is_cancelled() {
                    let _ = Self::shutdown_nak(&mut delivery).await;
                    error
                } else {
                    let error = self
                        .classify_pre_turn_failure(
                            &activation,
                            delivery.failure_key(),
                            &lease,
                            error,
                        )
                        .await;
                    if super::daemon_session_exec::is_durable_activation_error(&error) {
                        let _ = self
                            .terminate_activation(&mut delivery, "durably failed")
                            .await;
                    } else {
                        let _ =
                            Self::delayed_nak(&mut delivery, ActivationNakReason::PreparationError)
                                .await;
                    }
                    error
                };
                let _ = lease.release().await;
                return Err(error);
            }
        };
        self.prepare_session_services(&activation, &abort_signal, &shutdown)
            .await;
        Ok(PreparedActivation {
            execution,
            delivery,
            activation,
            lease,
            abort_signal,
            control,
            shutdown,
            span,
            lease_acquired_at,
        })
    }

    async fn prepare_session_services(
        self: &Arc<Self>,
        activation: &SessionActivate,
        abort: &crate::utils::AbortSignal,
        shutdown: &tokio_util::sync::CancellationToken,
    ) {
        let worker = Arc::clone(self);
        let activation = activation.clone();
        let stopped = abort.clone();
        let lifecycle = shutdown.clone();
        let mut startup = tokio::spawn(async move {
            worker.start_session_tool_servers(&activation).await;
            if stopped.aborted() || lifecycle.is_cancelled() {
                worker
                    .end_session_tool_servers(&activation.session_id)
                    .await;
            }
        });
        tokio::select! {
            _ = crate::utils::wait_abort_signal(abort) => {
                // Keep the registration task: its final release repairs claims
                // that arrive after the interruption was already accepted.
                tokio::spawn(async move { let _ = startup.await; });
                return;
            }
            _ = shutdown.cancelled() => {
                tokio::spawn(async move { let _ = startup.await; });
                return;
            }
            _ = &mut startup => {},
        }
        tokio::select! {
            _ = crate::utils::wait_abort_signal(abort) => {},
            _ = shutdown.cancelled() => {},
            _ = super::daemon_background::await_initial_background_services(&self.background_services_attempted) => {},
        }
    }

    fn active_session_started(activation: &SessionActivate, lease: &NatsSessionLease) {
        nats_metrics::active_session_started();
        let snapshot = nats_metrics::snapshot();
        log::info!(
            "active session started: session_id={} worker_id={} revision={} active_sessions_per_worker={}",
            activation.session_id,
            lease.worker_id(),
            lease.fence_token(),
            snapshot.active_sessions_per_worker
        );
    }

    fn active_session_finished(session_id: &str, lease: &NatsSessionLease) {
        nats_metrics::active_session_finished();
        let snapshot = nats_metrics::snapshot();
        log::info!(
            "active session finished: session_id={} worker_id={} revision={} active_sessions_per_worker={}",
            session_id,
            lease.worker_id(),
            lease.fence_token(),
            snapshot.active_sessions_per_worker

        );
    }
    async fn settle_session_delivery(
        &self,
        delivery: &mut ActivationDelivery,
        result: &Result<FinishCause>,
    ) {
        match result.as_ref() {
            Ok(cause) if cause.is_terminal() => {
                let _ = self.acknowledge_activation(delivery, "completed").await;
            }
            Ok(FinishCause::Failover(_)) => {
                let _ = Self::shutdown_nak(delivery).await;
                self.flush_shutdown_disposition().await;
            }
            Err(error) if super::daemon_session_exec::is_durable_activation_error(error) => {
                let _ = self
                    .terminate_activation(delivery, durable_termination_reason(error))
                    .await;
            }
            _ => {
                let _ = Self::delayed_nak(delivery, ActivationNakReason::SettlementRejection).await;
            }
        }
    }
    async fn run_session_task(self: &Arc<Self>, prepared: PreparedActivation) {
        let PreparedActivation {
            activation,
            lease,
            abort_signal,
            control,
            execution,
            mut delivery,
            shutdown,
            span,
            lease_acquired_at,
        } = prepared;
        let worker = Arc::clone(self);
        let session_id = activation.session_id.clone();
        let task_session_id = session_id.clone();
        harnx_metrics::record_activation_phase("lease_to_turn_start", lease_acquired_at.elapsed());
        tracing::info!(event = "activation_turn_started", session_id = %activation.session_id,
            activation_id = %activation.epoch, agent = activation.agent_name.as_deref().unwrap_or("unknown"), cluster = %self.cluster,
            delivery_attempt = delivery.message().info().map_or(0, |i| i.delivered),
            elapsed_ms = lease_acquired_at.elapsed().as_millis() as u64, reason = "started",
            "activation turn started");
        Self::active_session_started(&activation, &lease);
        async move {
            let result = if shutdown.is_cancelled() {
                control.task.abort();
                let _ = control.task.await;
                let backend = NatsSessionLogBackend::new(
                    worker.jetstream.clone(),
                    &activation.session_id,
                    worker.lease.replicas,
                );
                let turn_config = Arc::new(ConfigLock::new(worker.config.read().clone()));
                execution
                    .finish(
                        &backend,
                        &lease,
                        super::execution_control::FinishedTurn::for_failover(
                            None,
                            turn_config,
                            super::execution_control::FailoverCause::Shutdown,
                        ),
                    )
                    .await
            } else {
                worker
                    .execute_session(super::daemon_session_exec::SessionExecutionInputs {
                        activation,
                        activation_failure_key: delivery.failure_key().to_string(),
                        lease: Arc::clone(&lease),
                        abort_signal,
                        control_task: control.task,
                        hitl_decision_rx: control.hitl_decision_rx,
                        execution,
                        shutdown,
                    })
                    .await
            };
            worker.end_session_tool_servers(&task_session_id).await;
            worker.settle_session_delivery(&mut delivery, &result).await;
            if result.is_err() {
                let _ = lease.release().await;
            }
            Self::active_session_finished(&task_session_id, &lease);
            if let Err(error) = result {
                log::warn!("worker session execution failed for {task_session_id}: {error:#}");
            }
        }
        .instrument(span)
        .await;
    }

    async fn claim_activation(
        &self,
        delivery: &mut ActivationDelivery,
        origin: ClaimOrigin,
    ) -> Result<ClaimOutcome> {
        if self.shutdown.is_cancelled() {
            Self::shutdown_nak(delivery).await?;
            return Ok(ClaimOutcome::Settled);
        }
        if self
            .activation_faults
            .as_ref()
            .is_some_and(|faults| faults.fail_claims.load(std::sync::atomic::Ordering::SeqCst))
        {
            // Test seam: exercise the claim-error disposition paths
            // deterministically (handler and busy-waiter wake alike).
            return Err(anyhow::anyhow!("injected activation claim failure"));
        }
        let Some(activation) = self.decode_activation(delivery).await? else {
            return Ok(ClaimOutcome::Settled);
        };
        match self
            .reserve_activation(&activation.session_id, delivery.stream_sequence())
            .await
        {
            ReservationOutcome::Reserved(generation) => {
                let ctx = ClaimContext {
                    reservation_generation: generation,
                    origin,
                };
                match self
                    .claim_reserved_activation(delivery, activation, ctx)
                    .await?
                {
                    ClaimedOrBusy::Claimed(claimed) => Ok(ClaimOutcome::Claimed(claimed)),
                    ClaimedOrBusy::Busy(activation) => Ok(ClaimOutcome::Busy(activation)),
                    ClaimedOrBusy::Settled => Ok(ClaimOutcome::Settled),
                }
            }
            ReservationOutcome::SameDelivery => {
                metrics::counter!(harnx_metrics::ACTIVATION_CLAIMS_TOTAL, "outcome" => "same_delivery").increment(1);
                Ok(ClaimOutcome::Settled)
            }
            ReservationOutcome::DistinctDelivery => {
                // This worker is already running (or claiming) this session
                // with a different delivery. The caller parks the delivery on
                // a busy waiter instead of handing it back to JetStream.
                metrics::counter!(harnx_metrics::ACTIVATION_CLAIMS_TOTAL, "outcome" => "busy")
                    .increment(1);
                Ok(ClaimOutcome::Busy(activation))
            }
        }
    }

    async fn claim_reserved_activation(
        &self,
        delivery: &mut ActivationDelivery,
        activation: SessionActivate,
        ctx: ClaimContext,
    ) -> Result<ClaimedOrBusy> {
        let session_id = activation.session_id.clone();
        let result = self.attempt_reserved_claim(delivery, activation, ctx).await;
        if !matches!(result, Ok(ClaimedOrBusy::Claimed(_))) {
            self.release_activation_reservation(&session_id, ctx.reservation_generation)
                .await;
            if ctx.origin == ClaimOrigin::Admission {
                // The claim gave up its Starting reservation without ever
                // holding a lease, so no lease-key Delete will ever signal
                // that this worker is done with the session. Wake any parked
                // waiters now instead of letting them wait out a recheck
                // interval.
                self.notify_session_released(&session_id).await;
            }
            if matches!(result, Ok(ClaimedOrBusy::Busy(_))) {
                self.stop_busy_claim_heartbeat(delivery).await;
            }
        }
        result
    }

    /// The claim attempt itself: preflight, the lease CAS, and building the
    /// claimed activation. Every early exit settles the delivery or returns
    /// it busy; the caller owns the reservation epilogue.
    async fn attempt_reserved_claim(
        &self,
        delivery: &mut ActivationDelivery,
        activation: SessionActivate,
        ctx: ClaimContext,
    ) -> Result<ClaimedOrBusy> {
        let admission_started = std::time::Instant::now();
        let span =
            agent_activation_span(delivery.message().headers.as_ref(), &activation.session_id);
        match self
            .activation_is_ready(delivery, &activation, ctx.reservation_generation)
            .await?
        {
            PreflightOutcome::Busy => {
                return Ok(ClaimedOrBusy::Busy(activation));
            }
            PreflightOutcome::Finished => {
                metrics::counter!(harnx_metrics::ACTIVATION_CLAIMS_TOTAL, "outcome" => "preflight_not_ready").increment(1);
                return Ok(ClaimedOrBusy::Settled);
            }
            PreflightOutcome::Ready => {}
        }
        if self.shutdown.is_cancelled() {
            Self::shutdown_nak(delivery).await?;
            return Ok(ClaimedOrBusy::Settled);
        }
        let lease = match self
            .acquire_or_defer_activation(delivery, &activation, ctx.reservation_generation)
            .await?
        {
            LeaseAcquisition::Acquired(lease) => lease,
            LeaseAcquisition::Held => {
                // The lease CAS lost: someone else holds the session. The
                // caller parks the delivery instead of NAKing it back.
                return Ok(ClaimedOrBusy::Busy(activation));
            }
            LeaseAcquisition::Settled => {
                // A targeted acquisition error already NAKed the delivery
                // back; the claim ends here and must not park it.
                return Ok(ClaimedOrBusy::Settled);
            }
        };
        let lease_acquired_at = std::time::Instant::now();
        harnx_metrics::record_activation_phase("admission_to_lease", admission_started.elapsed());
        metrics::counter!(harnx_metrics::ACTIVATION_CLAIMS_TOTAL, "outcome" => "claimed")
            .increment(1);
        tracing::info!(event = "activation_lease_acquired", session_id = %activation.session_id,
            activation_id = %activation.epoch, agent = activation.agent_name.as_deref().unwrap_or("unknown"), cluster = %self.cluster,
            delivery_attempt = delivery.message().info().map_or(0, |i| i.delivered),
            elapsed_ms = admission_started.elapsed().as_millis() as u64, reason = "claimed",
            "activation lease acquired");
        delivery.attach_lease(&lease);
        if self.shutdown.is_cancelled() {
            lease.release().await?;
            Self::shutdown_nak(delivery).await?;
            return Ok(ClaimedOrBusy::Settled);
        }
        Ok(ClaimedOrBusy::Claimed(ClaimedActivation {
            activation,
            lease,
            reservation_generation: ctx.reservation_generation,
            span,
            lease_acquired_at,
        }))
    }

    /// Test seam: hand a busy result back with the delivery's heartbeat
    /// already stopped — what a wind-up lease lost mid-claim leaves behind —
    /// so the dead-heartbeat park rejections are exercised end to end.
    async fn stop_busy_claim_heartbeat(&self, delivery: &mut ActivationDelivery) {
        if self.activation_faults.as_ref().is_some_and(|faults| {
            faults
                .busy_claims_dead_heartbeat
                .load(std::sync::atomic::Ordering::SeqCst)
        }) {
            delivery.stop_heartbeat().await;
        }
    }

    pub(super) async fn handle_activation(
        self: &Arc<Self>,
        message: async_nats::jetstream::Message,
        delivered_wall: time::OffsetDateTime,
    ) -> Result<()> {
        let received_at = std::time::Instant::now();
        if let Ok(info) = message.info() {
            let attempt = if info.delivered > 1 {
                "redelivery"
            } else {
                "first"
            };
            metrics::counter!(harnx_metrics::ACTIVATIONS_RECEIVED_TOTAL, "attempt" => attempt)
                .increment(1);
            if info.delivered > 1 {
                metrics::counter!(harnx_metrics::ACTIVATION_REDELIVERIES_TOTAL).increment(1);
            }
            record_publish_to_delivery(delivered_wall, info.published);
        }
        let mut delivery =
            ActivationDelivery::start(message, self.activation_heartbeat_interval).await?;
        if let Ok(activation) =
            serde_json::from_slice::<SessionActivate>(&delivery.message().payload)
        {
            tracing::info!(event = "activation_delivered", session_id = %activation.session_id,
                activation_id = %activation.epoch, agent = activation.agent_name.as_deref().unwrap_or("unknown"), cluster = %self.cluster,
                delivery_attempt = delivery.message().info().map_or(0, |i| i.delivered),
                elapsed_ms = delivery.message().info().ok()
                    .and_then(|i| std::time::Duration::try_from(delivered_wall - i.published).ok())
                    .map_or(0, |duration| duration.as_millis() as u64), reason = "delivered",
                "activation delivered");
            tracing::info!(event = "activation_admitted", session_id = %activation.session_id,
                activation_id = %activation.epoch, agent = activation.agent_name.as_deref().unwrap_or("unknown"), cluster = %self.cluster,
                delivery_attempt = delivery.message().info().map_or(0, |i| i.delivered),
                elapsed_ms = std::time::Duration::try_from(time::OffsetDateTime::now_utc() - delivered_wall)
                    .map_or(0, |duration| duration.as_millis() as u64), reason = "admitted",
                "activation admitted");
        }
        match self
            .claim_activation(&mut delivery, ClaimOrigin::Admission)
            .await
        {
            Ok(ClaimOutcome::Claimed(claimed)) => {
                self.launch_claimed_session(claimed, delivery).await
            }
            Ok(ClaimOutcome::Busy(activation)) => {
                // The delivery moves into the parked busy waiter, which keeps
                // its progress heartbeats going. Returning here releases the
                // admission permit: waiting on a busy session must never hold
                // one.
                self.park_busy_activation(activation, delivery).await
            }
            Ok(ClaimOutcome::Settled) => {
                if let Ok(activation) =
                    serde_json::from_slice::<SessionActivate>(&delivery.message().payload)
                {
                    tracing::info!(event = "activation_claim_deferred", session_id = %activation.session_id,
                        activation_id = %activation.epoch, agent = activation.agent_name.as_deref().unwrap_or("unknown"),
                        cluster = %self.cluster,
                        delivery_attempt = delivery.message().info().map_or(0, |i| i.delivered),
                        elapsed_ms = received_at.elapsed().as_millis() as u64,
                        reason = "not_claimed", "activation not claimed");
                }
                delivery.stop_heartbeat().await;
                Ok(())
            }
            Err(error) => {
                if let Ok(activation) =
                    serde_json::from_slice::<SessionActivate>(&delivery.message().payload)
                {
                    tracing::info!(event = "activation_claim_failed", session_id = %activation.session_id,
                        activation_id = %activation.epoch, agent = activation.agent_name.as_deref().unwrap_or("unknown"), cluster = %self.cluster,
                        delivery_attempt = delivery.message().info().map_or(0, |i| i.delivered),
                        elapsed_ms = received_at.elapsed().as_millis() as u64,
                        reason = "error", "activation claim failed");
                }
                metrics::counter!(harnx_metrics::ACTIVATION_CLAIMS_TOTAL, "outcome" => "error")
                    .increment(1);
                let disposition = if self.shutdown.is_cancelled() {
                    Self::shutdown_nak(&mut delivery).await
                } else {
                    Self::delayed_nak(&mut delivery, ActivationNakReason::ClaimError).await
                };
                disposition?;
                Err(error)
            }
        }
    }

    /// Hand a claimed activation to its session supervisor task. Shared by
    /// admission (`handle_activation`) and by parked busy waiters that
    /// claimed after their session freed up.
    async fn launch_claimed_session(
        self: &Arc<Self>,
        claimed: ClaimedActivation,
        delivery: ActivationDelivery,
    ) -> Result<()> {
        let worker = Arc::clone(self);
        let session_id = claimed.activation.session_id.clone();
        let task_session_id = session_id.clone();
        let reservation_generation = claimed.reservation_generation;
        let shutdown = self.shutdown.child_token();
        let task_shutdown = shutdown.clone();
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            if start_rx.await.is_ok() {
                let prepared = worker
                    .prepare_claimed_activation(claimed, delivery, task_shutdown)
                    .await;
                match prepared {
                    Ok(prepared) => worker.run_session_task(prepared).await,
                    Err(error) => {
                        log::warn!("session preparation failed: {error:#}");
                    }
                }
            }
            worker
                .release_activation_tracking(&task_session_id, reservation_generation)
                .await;
        });
        self.active.lock().await.insert(
            session_id.clone(),
            ActiveSession {
                reservation_generation,
                shutdown,
                join: handle,
            },
        );
        anyhow::ensure!(
            self.mark_activation_running(&session_id, reservation_generation)
                .await,
            "claimed activation reservation must transition from starting to running"
        );
        let _ = start_tx.send(());
        Ok(())
    }

    /// Subscribe to control before spawning its listener task.
    ///
    /// Returning only after `subscribe` completes is the ordering barrier used by
    /// activation handling before it acknowledges the non-durable work message.
    async fn spawn_control_listener(ctx: ControlListenerCtx<'_>) -> Result<PreparedControl> {
        let ctrl_subject = control_subject(ctx.session_id);
        let subscriber = ctx
            .client
            .subscribe(ctrl_subject)
            .await
            .context("subscribe to session control subject")?;
        // Flush the SUB protocol command so broker-side interest exists before
        // activation is acknowledged and clients can observe that readiness.
        ctx.client
            .flush()
            .await
            .context("flush session control subscription")?;
        let (hitl_decision_tx, hitl_decision_rx) = tokio::sync::mpsc::unbounded_channel();
        let handler = SessionControlHandler::new(&ctx, hitl_decision_tx);
        Ok(PreparedControl {
            task: tokio::spawn(handler.listen(subscriber)),
            hitl_decision_rx,
        })
    }
}

#[cfg(test)]
mod tests {
    use async_nats::header::NATS_MESSAGE_ID;
    use opentelemetry::trace::{SpanId, SpanKind, TraceContextExt, TraceId};
    use std::time::Duration;

    use super::{agent_activation_span, busy_nak_delay, record_publish_to_delivery};

    #[test]
    fn broker_clock_ahead_records_zero_publish_to_delivery() {
        use metrics_util::{
            debugging::{DebugValue, DebuggingRecorder},
            CompositeKey, MetricKind,
        };

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let now = time::OffsetDateTime::now_utc();
            assert_eq!(
                record_publish_to_delivery(now, now + time::Duration::seconds(5)),
                std::time::Duration::ZERO
            );
        });
        let expected = CompositeKey::new(
            MetricKind::Histogram,
            metrics::Key::from_parts(
                harnx_metrics::ACTIVATION_PHASE_SECONDS,
                vec![metrics::Label::new("phase", "publish_to_delivery")],
            ),
        );
        assert!(snapshotter
            .snapshot()
            .into_vec()
            .iter()
            .any(|(key, _, _, value)| {
                key == &expected && *value == DebugValue::Histogram(vec![0.0.into()])
            }));
    }

    const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
    const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn activation_headers_continue_publisher_trace_at_consumer() {
        harnx_core::require_nextest();
        let spans = harnx_telemetry::collect_test_spans(|| {
            let mut upstream_headers = async_nats::HeaderMap::new();
            upstream_headers.insert("traceparent", TRACEPARENT);
            let publisher_parent =
                harnx_telemetry::propagate::extract_context_from_nats(&upstream_headers);
            let publisher_span = tracing::info_span!("activation_publisher");
            harnx_telemetry::set_span_parent(&publisher_span, publisher_parent);

            let headers = {
                let _entered = publisher_span.enter();
                super::super::activation_transport::activation_headers(
                    async_nats::header::HeaderValue::from("message-id"),
                )
            };
            assert_eq!(
                headers
                    .get(NATS_MESSAGE_ID)
                    .expect("activation message ID")
                    .as_str(),
                "message-id"
            );

            let extracted = harnx_telemetry::propagate::extract_context_from_nats(&headers);
            assert_eq!(
                extracted.span().span_context().trace_id(),
                TraceId::from_hex(TRACE_ID).expect("fixed trace ID")
            );
            drop(agent_activation_span(Some(&headers), "session-id"));
        });

        let publisher = spans
            .iter()
            .find(|span| span.name == "activation_publisher")
            .expect("publisher span");
        let consumer = spans
            .iter()
            .find(|span| span.name == "agent_activation")
            .expect("consumer span");
        assert_eq!(consumer.span_kind, SpanKind::Consumer);
        assert_eq!(
            consumer.span_context.trace_id(),
            publisher.span_context.trace_id()
        );
        assert_eq!(consumer.parent_span_id, publisher.span_context.span_id());
    }

    #[test]
    fn activation_without_headers_starts_new_root() {
        harnx_core::require_nextest();
        let spans = harnx_telemetry::collect_test_spans(|| {
            drop(agent_activation_span(None, "session-id"));
        });

        let consumer = spans
            .iter()
            .find(|span| span.name == "agent_activation")
            .expect("consumer span");
        assert_eq!(consumer.span_kind, SpanKind::Consumer);
        assert_ne!(consumer.span_context.trace_id(), TraceId::INVALID);
        assert_eq!(consumer.parent_span_id, SpanId::INVALID);
    }

    #[test]
    fn busy_nak_uses_ten_second_base_and_caps_jitter_at_two_seconds() {
        harnx_core::require_nextest();
        assert_eq!(busy_nak_delay(Duration::ZERO), Duration::from_secs(10));
        assert_eq!(
            busy_nak_delay(Duration::from_millis(1_250)),
            Duration::from_millis(11_250)
        );
        assert_eq!(
            busy_nak_delay(Duration::from_secs(9)),
            Duration::from_secs(12)
        );
    }
}
