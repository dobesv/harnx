//! Per-worker runtime state and `SessionActivate` handling: lease
//! acquisition, this session's tool-server refcount, control-plane
//! subscription, and handing the claimed session off to execution.

mod activation_preflight;

use super::backend::NatsSessionLogBackend;
use super::control::{control_subject, SessionControlHandler};
use super::daemon::{SessionActivate, SessionActivationRoute, WorkerActivationMode};
use super::daemon_background::BackgroundServices;
use super::execution_control::{FinishCause, WorkerExecution};
use super::server_reconciler::{tool_servers_for_activation, ServerReconciler};
use crate::config::GlobalConfig;
use crate::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
use crate::nats_metrics;
use anyhow::{Context, Result};
use async_nats::jetstream;
use async_nats::jetstream::AckKind;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
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
/// redelivers an unacked activation at `WORK_NOTIFY_ACK_WAIT` with
/// `max_deliver: -1` — a session that never manages to ack loops forever,
/// each redelivery re-acquiring the lease and fencing the still-running
/// previous attempt.
pub(super) const SESSION_TOOL_SERVER_START_TIMEOUT: Duration = Duration::from_secs(20);

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

/// Borrowed parameters for [`WorkerRuntime::prepare_and_ack_activation`].
struct ActivationAckCtx<'a> {
    activation: &'a SessionActivate,
    message: &'a async_nats::jetstream::Message,
    lease: &'a Arc<NatsSessionLease>,
    abort_signal: &'a crate::utils::AbortSignal,
}

struct ClaimedActivation {
    activation: SessionActivate,
    lease: Arc<NatsSessionLease>,
    span: tracing::Span,
}

struct PreparedActivation {
    activation: SessionActivate,
    lease: Arc<NatsSessionLease>,
    abort_signal: crate::utils::AbortSignal,
    control: PreparedControl,
    execution: WorkerExecution,
    message: async_nats::jetstream::Message,
    shutdown: tokio_util::sync::CancellationToken,
    span: tracing::Span,
}

pub(super) struct ActiveSession {
    shutdown: tokio_util::sync::CancellationToken,
    join: JoinHandle<()>,
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
    pub(super) manage_servers: bool,
    pub(super) worker_id: String,
    pub(super) identity: crate::worker_identity::WorkerReadiness,
    pub(super) lease: NatsLeaseConfig,
    pub(super) jetstream: jetstream::Context,
    pub(super) session_metadata: crate::nats_session_metadata::SessionMetadataStore,
    /// Shared NATS client for control-plane subscriptions (cloned per session
    /// rather than reconnecting on each activation).
    pub(super) client: async_nats::Client,
    pub(super) call_fn: Option<crate::agent_loop::AgentCallFn>,
    pub(super) generation: AtomicU64,
    pub(super) shutdown: tokio_util::sync::CancellationToken,
    pub(super) active: Mutex<HashMap<String, ActiveSession>>,
}

impl WorkerRuntime {
    fn uses_targeted_activation(&self) -> bool {
        self.activation_mode == WorkerActivationMode::WorkerTargeted
    }

    pub(super) async fn already_running(&self, session_id: &str) -> bool {
        let mut active = self.active.lock().await;
        active.retain(|_, session| !session.join.is_finished());
        active.contains_key(session_id)
    }

    /// Close admission's active-session side and await every single-owner
    /// supervisor. Session supervisors retain their message, lease, and NATS
    /// client until failover cleanup completes.
    pub(super) async fn shutdown_active_sessions(&self) {
        let sessions = {
            let mut active = self.active.lock().await;
            active
                .drain()
                .map(|(_, session)| session)
                .collect::<Vec<_>>()
        };
        for session in &sessions {
            session.shutdown.cancel();
        }
        for session in sessions {
            if let Err(error) = session.join.await {
                log::warn!("worker session supervisor failed during shutdown: {error}");
            }
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
                    activation.session_id
                );
            }
            Err(_) => {
                log::warn!(
                    "session '{}' tool-server startup ({}) exceeded {}s; continuing this \
                     activation without waiting further (still starting in the background)",
                    activation.session_id,
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
    ) -> Result<Option<Arc<NatsSessionLease>>> {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst);
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

    async fn shutdown_nak(message: &async_nats::jetstream::Message) -> Result<()> {
        message
            .ack_with(AckKind::Nak(None))
            .await
            .map_err(|error| anyhow::anyhow!("NAK SessionActivate during shutdown: {error}"))
    }

    async fn delayed_nak(message: &async_nats::jetstream::Message) -> Result<()> {
        let delivered = message.info().map(|info| info.delivered).unwrap_or(1);
        let exponent = u32::try_from(delivered.saturating_sub(1))
            .unwrap_or(u32::MAX)
            .min(5);
        let millis = 100_u64.saturating_mul(1_u64 << exponent).min(2_000);
        message
            .ack_with(AckKind::Nak(Some(Duration::from_millis(millis))))
            .await
            .map_err(|error| anyhow::anyhow!("delayed-NAK targeted SessionActivate: {error}"))
    }

    async fn flush_shutdown_disposition(&self) {
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

    async fn targeted_activation_is_covered(&self, activation: &SessionActivate) -> Result<bool> {
        let requested_seq = activation
            .requested_seq
            .context("targeted activation is missing requested_seq")?;
        let backend = NatsSessionLogBackend::new(
            self.jetstream.clone(),
            &activation.session_id,
            self.lease.replicas,
        );
        let entries = backend.load_events_latest_async().await?;
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

    async fn terminate_activation(
        message: &async_nats::jetstream::Message,
        reason: &str,
    ) -> Result<()> {
        message
            .ack_with(AckKind::Term)
            .await
            .map_err(|error| anyhow::anyhow!("terminate {reason} SessionActivate: {error}"))
    }

    async fn decode_activation(
        &self,
        message: &async_nats::jetstream::Message,
    ) -> Result<Option<SessionActivate>> {
        match serde_json::from_slice(&message.payload) {
            Ok(activation) => Ok(Some(activation)),
            Err(error) if self.uses_targeted_activation() => {
                log::warn!("terminating malformed targeted SessionActivate: {error}");
                Self::terminate_activation(message, "malformed targeted").await?;
                Ok(None)
            }
            Err(error) => Err(error).context("decode SessionActivate"),
        }
    }

    async fn targeted_status_preflight_finished(
        &self,
        message: &async_nats::jetstream::Message,
        activation: &SessionActivate,
    ) -> Result<bool> {
        match self.targeted_activation_is_covered(activation).await {
            Ok(true) => {
                message
                    .ack()
                    .await
                    .map_err(|error| anyhow::anyhow!("ack covered targeted activation: {error}"))?;
                Ok(true)
            }
            Ok(false) => Ok(false),
            Err(error) => {
                log::warn!(
                    "targeted activation status read failed for session '{}': {error:#}",
                    activation.session_id
                );
                Self::delayed_nak(message).await?;
                Ok(true)
            }
        }
    }

    async fn settle_running_activation(
        &self,
        message: &async_nats::jetstream::Message,
    ) -> Result<()> {
        Self::delayed_nak(message).await
    }

    async fn acquire_or_defer_activation(
        &self,
        message: &async_nats::jetstream::Message,
        activation: &SessionActivate,
    ) -> Result<Option<Arc<NatsSessionLease>>> {
        match self.acquire_activation_lease(activation).await {
            Ok(Some(lease)) => Ok(Some(lease)),
            Ok(None) => {
                Self::delayed_nak(message).await?;
                Ok(None)
            }
            Err(error) if self.uses_targeted_activation() => {
                log::warn!(
                    "targeted activation lease attempt failed for session '{}': {error:#}",
                    activation.session_id
                );
                Self::delayed_nak(message).await?;
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    async fn prepare_activation_control(
        &self,
        ctx: &ActivationAckCtx<'_>,
    ) -> Result<PreparedControl> {
        let ActivationAckCtx {
            activation,
            lease,
            abort_signal,
            ..
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
            Err(error) => {
                let _ = lease.release().await;
                Err(error)
            }
        }
    }

    /// Subscribe control and acknowledge the activation, in that order.
    /// `start_session_tool_servers` already registered this session as a tool-
    /// server user before either step ran; no task exists yet to release that
    /// on completion (that happens inside the spawned task `handle_activation`
    /// creates once this returns `Ok`), so either failure here must release it
    /// itself or the server stays pinned running for the rest of the worker's
    /// lifetime.
    async fn prepare_and_ack_activation(
        &self,
        ctx: ActivationAckCtx<'_>,
    ) -> Result<PreparedControl> {
        let control = match self.prepare_activation_control(&ctx).await {
            Ok(control) => control,
            Err(error) => {
                self.end_session_tool_servers(&ctx.activation.session_id)
                    .await;
                return Err(error);
            }
        };
        if let Err(error) = ctx.message.ack_with(AckKind::Progress).await {
            control.task.abort();
            let _ = ctx.lease.release().await;
            self.end_session_tool_servers(&ctx.activation.session_id)
                .await;
            return Err(anyhow::anyhow!("ack SessionActivate: {error}"));
        }
        Ok(control)
    }

    async fn prepare_claimed_activation(
        self: &Arc<Self>,
        claimed: ClaimedActivation,
        message: &async_nats::jetstream::Message,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<PreparedActivation> {
        let ClaimedActivation {
            activation,
            lease,
            span,
        } = claimed;
        let execution = WorkerExecution::claim(&activation, &lease);
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

        let abort_signal = crate::utils::create_abort_signal();
        let control = self
            .prepare_and_ack_activation(ActivationAckCtx {
                activation: &activation,
                message,
                lease: &lease,
                abort_signal: &abort_signal,
            })
            .await?;
        self.prepare_session_services(&activation, &abort_signal, &shutdown)
            .await;
        Ok(PreparedActivation {
            execution,
            message: message.clone(),
            activation,
            lease,
            abort_signal,
            control,
            shutdown,
            span,
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
    async fn run_session_task(self: &Arc<Self>, prepared: PreparedActivation) {
        let PreparedActivation {
            activation,
            lease,
            abort_signal,
            control,
            execution,
            message,
            shutdown,
            span,
        } = prepared;
        let worker = Arc::clone(self);
        let session_id = activation.session_id.clone();
        let task_session_id = session_id.clone();
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
                let turn_config = Arc::new(parking_lot::RwLock::new(worker.config.read().clone()));
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
                        lease: Arc::clone(&lease),
                        abort_signal,
                        control_task: control.task,
                        hitl_decision_rx: control.hitl_decision_rx,
                        execution,
                        shutdown,
                    })
                    .await
            };
            match result.as_ref() {
                Ok(cause) if cause.is_terminal() => {
                    let _ = message.ack().await;
                }
                Ok(FinishCause::Failover(_)) => {
                    let _ = Self::shutdown_nak(&message).await;
                    worker.flush_shutdown_disposition().await;
                }
                _ => {
                    let _ = Self::delayed_nak(&message).await;
                }
            }
            worker.end_session_tool_servers(&task_session_id).await;
            Self::active_session_finished(&task_session_id, &lease);
            if let Err(error) = result {
                log::warn!("worker session execution failed: {error:#}");
            }
        }
        .instrument(span)
        .await;
    }

    async fn claim_activation(
        &self,
        message: &async_nats::jetstream::Message,
    ) -> Result<Option<ClaimedActivation>> {
        if self.shutdown.is_cancelled() {
            Self::shutdown_nak(message).await?;
            return Ok(None);
        }
        let Some(activation) = self.decode_activation(message).await? else {
            return Ok(None);
        };
        let span = agent_activation_span(message.headers.as_ref(), &activation.session_id);
        if !self.activation_is_ready(message, &activation).await? {
            return Ok(None);
        }
        if self.shutdown.is_cancelled() {
            Self::shutdown_nak(message).await?;
            return Ok(None);
        }
        let Some(lease) = self
            .acquire_or_defer_activation(message, &activation)
            .await?
        else {
            return Ok(None);
        };
        if self.shutdown.is_cancelled() {
            lease.release().await?;
            Self::shutdown_nak(message).await?;
            return Ok(None);
        }
        Ok(Some(ClaimedActivation {
            activation,
            lease,
            span,
        }))
    }

    pub(super) async fn handle_activation(
        self: &Arc<Self>,
        message: async_nats::jetstream::Message,
    ) -> Result<()> {
        let Some(claimed) = self.claim_activation(&message).await? else {
            return Ok(());
        };
        let worker = Arc::clone(self);
        let session_id = claimed.activation.session_id.clone();
        let shutdown = self.shutdown.child_token();
        let task_shutdown = shutdown.clone();
        let handle = tokio::spawn(async move {
            let prepared = worker
                .prepare_claimed_activation(claimed, &message, task_shutdown)
                .await;
            match prepared {
                Ok(prepared) => worker.run_session_task(prepared).await,
                Err(error) => {
                    log::warn!("session preparation failed: {error:#}");
                    if worker.shutdown.is_cancelled() {
                        let _ = Self::shutdown_nak(&message).await;
                    } else {
                        let _ = Self::delayed_nak(&message).await;
                    }
                }
            }
        });
        self.active.lock().await.insert(
            session_id,
            ActiveSession {
                shutdown,
                join: handle,
            },
        );
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

    use super::agent_activation_span;

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
}
