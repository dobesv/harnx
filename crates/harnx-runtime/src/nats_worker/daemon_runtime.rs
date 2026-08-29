//! Per-worker runtime state and `SessionActivate` handling: lease
//! acquisition, this session's tool-server refcount, control-plane
//! subscription, and handing the claimed session off to execution.

use super::backend::NatsSessionLogBackend;
use super::control::{control_subject, ControlCommand};
use super::daemon::{
    should_append_control_log_entry, SessionActivate, SessionActivationRoute, WorkerActivationMode,
};
use super::daemon_background::BackgroundServices;
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
struct ControlListenerCtx<'a> {
    client: &'a async_nats::Client,
    session_id: &'a str,
    lease: &'a Arc<NatsSessionLease>,
    backend: &'a NatsSessionLogBackend,
    abort_signal: &'a crate::utils::AbortSignal,
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
    control_task: JoinHandle<()>,
    span: tracing::Span,
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
    pub(super) _background_services: Arc<Mutex<Option<BackgroundServices>>>,
    pub(super) background_services_attempted: tokio::sync::watch::Receiver<bool>,
    /// `None` for a consuming worker, or a managing worker with nothing
    /// configured to spawn anywhere.
    pub(super) server_reconciler: Option<Arc<ServerReconciler>>,
    #[allow(dead_code)]
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
    pub(super) active: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl WorkerRuntime {
    fn uses_targeted_activation(&self) -> bool {
        self.activation_mode == WorkerActivationMode::WorkerTargeted
    }

    pub(super) async fn already_running(&self, session_id: &str) -> bool {
        let mut active = self.active.lock().await;
        active.retain(|_, handle| !handle.is_finished());
        active.contains_key(session_id)
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
        let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: self.jetstream.clone(),
            session_id: &activation.session_id,
            worker_id: self.worker_id.clone(),
            generation,
            config: self.lease.clone(),
            session_metadata: Some(self.session_metadata.clone()),
        })
        .await?;
        Ok(lease.map(Arc::new))
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

    async fn targeted_activation_is_covered(&self, activation: &SessionActivate) -> Result<bool> {
        let requested_seq = activation
            .requested_seq
            .context("targeted activation is missing requested_seq")?;
        let backend = NatsSessionLogBackend::new(self.jetstream.clone(), &activation.session_id);
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
        if self.uses_targeted_activation() {
            Self::delayed_nak(message).await
        } else {
            let _ = message.ack().await;
            Ok(())
        }
    }

    async fn acquire_or_defer_activation(
        &self,
        message: &async_nats::jetstream::Message,
        activation: &SessionActivate,
    ) -> Result<Option<Arc<NatsSessionLease>>> {
        match self.acquire_activation_lease(activation).await {
            Ok(Some(lease)) => Ok(Some(lease)),
            Ok(None) if self.uses_targeted_activation() => {
                Self::delayed_nak(message).await?;
                Ok(None)
            }
            Ok(None) => Ok(None),
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
        activation: &SessionActivate,
        lease: &Arc<NatsSessionLease>,
        abort_signal: &crate::utils::AbortSignal,
    ) -> Result<JoinHandle<()>> {
        let backend = NatsSessionLogBackend::new(self.jetstream.clone(), &activation.session_id);
        match Self::spawn_control_listener(ControlListenerCtx {
            client: &self.client,
            session_id: &activation.session_id,
            lease,
            backend: &backend,
            abort_signal,
        })
        .await
        {
            Ok(task) => Ok(task),
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
    ) -> Result<JoinHandle<()>> {
        let control_task = match self
            .prepare_activation_control(ctx.activation, ctx.lease, ctx.abort_signal)
            .await
        {
            Ok(task) => task,
            Err(error) => {
                self.end_session_tool_servers(&ctx.activation.session_id)
                    .await;
                return Err(error);
            }
        };
        if let Err(error) = ctx.message.ack().await {
            control_task.abort();
            let _ = ctx.lease.release().await;
            self.end_session_tool_servers(&ctx.activation.session_id)
                .await;
            return Err(anyhow::anyhow!("ack SessionActivate: {error}"));
        }
        Ok(control_task)
    }

    async fn prepare_claimed_activation(
        &self,
        claimed: ClaimedActivation,
        message: &async_nats::jetstream::Message,
    ) -> Result<PreparedActivation> {
        let ClaimedActivation {
            activation,
            lease,
            span,
        } = claimed;
        self.start_session_tool_servers(&activation).await;
        super::daemon_background::await_initial_background_services(
            &self.background_services_attempted,
        )
        .await;
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
        let control_task = self
            .prepare_and_ack_activation(ActivationAckCtx {
                activation: &activation,
                message,
                lease: &lease,
                abort_signal: &abort_signal,
            })
            .await?;
        Ok(PreparedActivation {
            activation,
            lease,
            abort_signal,
            control_task,
            span,
        })
    }

    async fn spawn_session_task(self: &Arc<Self>, prepared: PreparedActivation) {
        let PreparedActivation {
            activation,
            lease,
            abort_signal,
            control_task,
            span,
        } = prepared;
        let worker = Arc::clone(self);
        let session_id = activation.session_id.clone();
        let task_session_id = session_id.clone();
        nats_metrics::active_session_started();
        let handle = tokio::spawn(
            async move {
                let snapshot = nats_metrics::snapshot();
                log::info!(
                    "active session started: session_id={} worker_id={} revision={} active_sessions_per_worker={}",
                    activation.session_id,
                    lease.worker_id(),
                    lease.fence_token(),
                    snapshot.active_sessions_per_worker
                );
                let result = worker
                    .execute_session(activation, Arc::clone(&lease), abort_signal, control_task)
                    .await;
                worker.end_session_tool_servers(&task_session_id).await;
                nats_metrics::active_session_finished();
                let snapshot = nats_metrics::snapshot();
                log::info!(
                    "active session finished: session_id={} worker_id={} revision={} active_sessions_per_worker={}",
                    task_session_id,
                    lease.worker_id(),
                    lease.fence_token(),
                    snapshot.active_sessions_per_worker
                );
                if let Err(error) = result {
                    log::warn!("worker session execution failed: {error:#}");
                }
            }
            .instrument(span),
        );
        self.active.lock().await.insert(session_id, handle);
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

    pub(super) async fn handle_activation(
        self: &Arc<Self>,
        message: async_nats::jetstream::Message,
    ) -> Result<()> {
        let Some(activation) = self.decode_activation(&message).await? else {
            return Ok(());
        };
        let span = agent_activation_span(message.headers.as_ref(), &activation.session_id);
        if !self
            .metadata_preflight_passes(&message, &activation)
            .await?
        {
            return Ok(());
        }
        if !self
            .targeted_route_preflight_passes(&message, &activation)
            .await?
        {
            return Ok(());
        }

        // A targeted re-activation stays durable until the active loop's tool
        // boundary or final drain has covered the requested sequence.
        if self.already_running(&activation.session_id).await {
            self.settle_running_activation(&message).await?;
            return Ok(());
        }

        if self.uses_targeted_activation()
            && self
                .targeted_status_preflight_finished(&message, &activation)
                .await?
        {
            return Ok(());
        }

        // A targeted lease loser uses a short delayed NAK so handling returns
        // immediately while preserving the final-drain race closure.
        let Some(lease) = self
            .acquire_or_defer_activation(&message, &activation)
            .await?
        else {
            return Ok(());
        };

        // Core-NATS control is subscribed before this acknowledges the
        // activation. The spawned task owns cleanup of the session's servers.
        let prepared = self
            .prepare_claimed_activation(
                ClaimedActivation {
                    activation,
                    lease,
                    span,
                },
                &message,
            )
            .await?;
        self.spawn_session_task(prepared).await;
        Ok(())
    }

    /// Subscribe to control before spawning its listener task.
    ///
    /// Returning only after `subscribe` completes is the ordering barrier used by
    /// activation handling before it acknowledges the non-durable work message.
    async fn spawn_control_listener(ctx: ControlListenerCtx<'_>) -> Result<JoinHandle<()>> {
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
        let ctrl_abort = ctx.abort_signal.clone();
        let ctrl_lease = Arc::clone(ctx.lease);
        let ctrl_backend = ctx.backend.clone();
        Ok(tokio::spawn(async move {
            use futures_util::StreamExt;
            let mut messages = subscriber;
            while let Some(msg) = messages.next().await {
                // Parse the control command.
                let Ok(command) = ControlCommand::from_bytes(&msg.payload) else {
                    log::debug!("invalid control command payload, ignoring");
                    continue;
                };
                match command {
                    ControlCommand::Cancel => {
                        // Worker-originated: append Cancel entry (worker's fence token),
                        // THEN fire AbortSignal.
                        if should_append_control_log_entry(&ctrl_lease) {
                            let cancel_entry = harnx_core::session::SessionLogEntry::Cancel {
                                fence_token: ctrl_lease.fence_token(),
                            };
                            // Append BEFORE firing abort; if append fails, still abort (safe).
                            if let Err(e) = ctrl_backend.append_event_blocking(&cancel_entry) {
                                log::warn!("failed to append Cancel entry: {e}");
                            }
                        }
                        ctrl_abort.set_ctrlc();
                    }
                }
            }
        }))
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
