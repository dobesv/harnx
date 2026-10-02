//! Connection-owned tool reservations, without an LLM turn or execution lease.
//!
//! Callers must stop starting calls when the scope is unavailable and rebuild
//! their provider/catalog after a generation change. In-flight calls are never
//! replayed by this helper. Explicit `close` waits for cleanup; Drop requests
//! the same cleanup while the Tokio runtime is still running.

use crate::config::Config;
use crate::nats_session_metadata::SessionInitializer;
use crate::nats_worker::tool_reservation::{
    reserve_subject, targeted_reserve_subject, Release, Renew, Reserve, ReserveReply, Reserved,
    ToolReservationControl, ToolReservationControlReply, ToolReservationView,
    TOOL_RESERVATION_PROTOCOL_VERSION,
};
use crate::nats_worker::{LocalWorkerTarget, SessionActivationRoute};
use crate::{NatsSession, NatsSessionConfig};
use anyhow::{bail, ensure, Context, Result};
use harnx_core::abort::create_abort_signal;
use harnx_core::instance::ServerScope;
use harnx_core::session::Session;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

// Worker setup and server startup each have a 20-second budget. Override the
// NATS client's shorter default request timeout for this operation.
const RESERVE_TIMEOUT: Duration = Duration::from_secs(60);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// One atomic snapshot for validating a provider/catalog before starting calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolReservationState {
    /// Changes on invalidation and on every successful re-reserve, even if the
    /// worker returns the same scope. Initial ready generation is zero.
    pub generation: u64,
    /// None during recovery and after close. Don't start calls in that state.
    pub server_scope: Option<ServerScope>,
}

/// Owns a durable backing session, connection-local caller identity and renewer.
/// The session is retained after close under normal session retention policy.
pub struct ToolReservationHandle {
    config: Config,
    session: NatsSession,
    state: watch::Receiver<ToolReservationState>,
    stop: CancellationToken,
    renewer: Option<JoinHandle<Result<()>>>,
}

impl ToolReservationHandle {
    /// `config` is owned by this connection. Its NATS routing is authoritative:
    /// bootstrap must resolve the cluster before calling, including CLI overrides.
    /// This method does not re-read frontend routing from the environment.
    pub async fn open(
        mut config: Config,
        route: SessionActivationRoute,
        view: ToolReservationView,
    ) -> Result<Self> {
        let (session, client, subject) = create_backing_session(&config, route).await?;
        // No agent identity: the durable session was created with Inline source.
        // Don't inherit an active agent/session from the process-wide config.
        config.session = Some(Session {
            id: session.session_id().to_owned(),
            session_id: Some(session.session_id().to_owned()),
            ..Default::default()
        });
        let reservation = reserve(&client, &subject, session.storage_key(), &view).await?;
        let (sender, state) = watch::channel(ToolReservationState {
            generation: 0,
            server_scope: Some(ServerScope::from_string(&reservation.server_scope)),
        });
        let stop = CancellationToken::new();
        let renewer = tokio::spawn(
            Renewer {
                client,
                subject,
                session_key: session.storage_key().to_owned(),
                view,
                reservation,
                sender,
                stop: stop.clone(),
                ready: true,
            }
            .run(),
        );
        Ok(Self {
            config,
            session,
            state,
            stop,
            renewer: Some(renewer),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn session_id(&self) -> &str {
        self.session.session_id()
    }

    pub fn session_storage_key(&self) -> &str {
        self.session.storage_key()
    }

    /// Prefer `state()` when scope and generation need to be read together.
    pub fn server_scope(&self) -> Option<ServerScope> {
        self.state.borrow().server_scope.clone()
    }

    pub fn generation(&self) -> u64 {
        self.state.borrow().generation
    }

    pub fn state(&self) -> ToolReservationState {
        self.state.borrow().clone()
    }

    /// Watch invalidation as well as replacement. Notifications may coalesce;
    /// compare generations, not the number of notifications received.
    pub fn subscribe(&self) -> watch::Receiver<ToolReservationState> {
        self.state.clone()
    }

    /// Stop renewal/recovery, then best-effort release the current reservation.
    /// Idempotent. Invalidated reservations are left to expire via the worker TTL.
    /// Release failures are logged, not returned; renewer join errors still propagate.
    /// Cancelling this future still leaves the renewer doing best-effort cleanup.
    pub async fn close(&mut self) -> Result<()> {
        self.stop.cancel();
        if let Some(renewer) = self.renewer.take() {
            renewer.await.context("tool reservation renewer failed")??;
        }
        Ok(())
    }
}

impl Drop for ToolReservationHandle {
    fn drop(&mut self) {
        // The renewer owns its client and last reservation, not this handle.
        // Detach rather than abort so it can release after stopping renewal.
        self.stop.cancel();
    }
}

/// Create the durable identity/transcript before any worker claim is requested.
async fn create_backing_session(
    config: &Config,
    route: SessionActivationRoute,
) -> Result<(NatsSession, async_nats::Client, String)> {
    let cluster = config.default_cluster_key().to_owned();
    let subject = match &route {
        SessionActivationRoute::ClusterShared => reserve_subject(&cluster),
        SessionActivationRoute::WorkerTargeted {
            session_scope,
            worker_id,
        } => targeted_reserve_subject(LocalWorkerTarget::new(session_scope, worker_id)?),
    };
    let server = config.resolve_nats_server(&cluster).await?;
    let replicas = server.resolved_replicas();
    let client = Config::connect_nats_server(&server).await?;
    let session = NatsSession::new_with_resolved_options(
        NatsSessionConfig {
            cluster,
            initializer: SessionInitializer::inline("", Default::default(), Default::default()),
            session_id: None,
            activation_route: route,
        },
        replicas,
        client.clone(),
        async_nats::jetstream::new(client.clone()),
        create_abort_signal(),
        Duration::from_secs(config.nats_lease_acquisition_timeout_secs),
    )
    .await
    .context("failed to create tool reservation backing session")?;
    // Direct subagent calls append progress to their caller's transcript.
    // No prompt has created that stream for an idle reservation session.
    crate::nats_session_log::NatsSessionLog::new_with_replicas(
        session.jetstream().clone(),
        session.storage_key(),
        replicas,
    )
    .last_entry_async()
    .await
    .context("failed to create tool reservation backing transcript")?;
    Ok((session, client, subject))
}

async fn reserve(
    client: &async_nats::Client,
    subject: &str,
    session_key: &str,
    view: &ToolReservationView,
) -> Result<Reserved> {
    let request = Reserve::new(session_key, view.clone());
    let reply = client
        .send_request(
            subject.to_owned(),
            async_nats::Request::new()
                .payload(serde_json::to_vec(&request)?.into())
                .timeout(Some(RESERVE_TIMEOUT)),
        )
        .await
        .context("tool reservation reserve request failed")?;
    match serde_json::from_slice::<ReserveReply>(&reply.payload)? {
        ReserveReply::Reserved(reservation) => {
            ensure!(
                reservation.protocol_version == TOOL_RESERVATION_PROTOCOL_VERSION
                    && reservation.attempt_id == request.attempt_id,
                "tool reservation reply version or attempt ID mismatch"
            );
            ensure!(
                reservation.renew_after_ms > 0
                    && reservation.renew_after_ms < reservation.ttl_ms
                    && !reservation.server_scope.is_empty()
                    && !reservation.control_subject.is_empty()
                    && !reservation.reservation_id.is_empty(),
                "invalid tool reservation reply"
            );
            Ok(reservation)
        }
        ReserveReply::Error(error) => bail!(
            "tool reservation reserve: {:?}: {}",
            error.code,
            error.message
        ),
    }
}

async fn control(
    client: &async_nats::Client,
    reservation: &Reserved,
    operation: ToolReservationControl,
) -> Result<()> {
    let reply = client
        .send_request(
            reservation.control_subject.clone(),
            async_nats::Request::new()
                .payload(serde_json::to_vec(&operation)?.into())
                .timeout(Some(CONTROL_TIMEOUT)),
        )
        .await?;
    match serde_json::from_slice::<ToolReservationControlReply>(&reply.payload)? {
        ToolReservationControlReply::Ok(_) => Ok(()),
        ToolReservationControlReply::Error(error) => bail!(
            "tool reservation control: {:?}: {}",
            error.code,
            error.message
        ),
    }
}

async fn release(client: &async_nats::Client, reservation: &Reserved) -> Result<()> {
    control(
        client,
        reservation,
        ToolReservationControl::Release(Release {
            reservation_id: reservation.reservation_id.clone(),
        }),
    )
    .await
}

fn publish_state(sender: &watch::Sender<ToolReservationState>, scope: Option<ServerScope>) {
    sender.send_modify(|state| {
        state.generation += 1;
        state.server_scope = scope;
    });
}

/// Owns the current reservation and its recovery inputs independently of the handle.
struct Renewer {
    client: async_nats::Client,
    subject: String,
    session_key: String,
    view: ToolReservationView,
    reservation: Reserved,
    sender: watch::Sender<ToolReservationState>,
    stop: CancellationToken,
    ready: bool,
}

impl Renewer {
    async fn run(mut self) -> Result<()> {
        loop {
            let delay = if self.ready {
                Duration::from_millis(self.reservation.renew_after_ms)
            } else {
                RETRY_DELAY
            };
            tokio::select! {
                biased;
                _ = self.stop.cancelled() => break,
                _ = tokio::time::sleep(delay) => {}
            }
            let proceed = if self.ready {
                self.renew().await
            } else {
                self.recover().await
            };
            if !proceed {
                break;
            }
        }
        publish_state(&self.sender, None);
        self.release_best_effort().await;
        Ok(())
    }

    // False means cancellation won, not a protocol failure. Those invalidate
    // scope and retry on the next iteration, as before.
    async fn renew(&mut self) -> bool {
        let result = tokio::select! {
            biased;
            _ = self.stop.cancelled() => return false,
            result = control(&self.client, &self.reservation, ToolReservationControl::Renew(Renew {
                reservation_id: self.reservation.reservation_id.clone(),
            })) => result,
        };
        if let Err(error) = result {
            log::warn!("tool reservation lost; re-reserving: {error:#}");
            self.ready = false;
            publish_state(&self.sender, None);
        }
        true
    }

    async fn recover(&mut self) -> bool {
        // A cancelled/lost reserve reply may leave a claim on a worker.
        // There is no cross-worker dedup; those abandoned claims expire.
        let result = tokio::select! {
            biased;
            _ = self.stop.cancelled() => return false,
            result = reserve(&self.client, &self.subject, &self.session_key, &self.view) => result,
        };
        match result {
            Ok(replacement) => {
                let old = std::mem::replace(&mut self.reservation, replacement);
                self.ready = true;
                publish_state(
                    &self.sender,
                    Some(ServerScope::from_string(&self.reservation.server_scope)),
                );
                tokio::select! {
                    biased;
                    _ = self.stop.cancelled() => return false,
                    _ = release(&self.client, &old) => {}
                }
            }
            Err(error) => log::warn!("tool reservation retry failed: {error:#}"),
        }
        true
    }

    async fn release_best_effort(&self) {
        if !self.ready {
            // Renewal already failed. Don't wait on the invalidated worker at close.
            log::warn!(
                "skipping release of invalidated tool reservation {} on worker {}; claim will expire via TTL ({} ms)",
                self.reservation.reservation_id,
                self.reservation.worker_id,
                self.reservation.ttl_ms,
            );
        } else if let Err(error) = release(&self.client, &self.reservation).await {
            log::warn!(
                "tool reservation {} release failed on worker {}; claim will expire via TTL ({} ms): {error:#}",
                self.reservation.reservation_id,
                self.reservation.worker_id,
                self.reservation.ttl_ms,
            );
        }
    }
}
