//! Worker-owned reservation state, separate from activation admission.

use super::*;
use crate::nats_worker::daemon_background::tool_servers_for_view;
use crate::nats_worker::daemon_runtime::{WorkerRuntime, SESSION_TOOL_SERVER_START_TIMEOUT};
use crate::nats_worker::server_reconciler::ServerReconciler;
use anyhow::{ensure, Context, Result};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_util::task::AbortOnDropHandle;

const MAX_PENDING_RESERVES: usize = 64;

struct Handler {
    config: crate::config::GlobalConfig,
    metadata: crate::nats_session_metadata::SessionMetadataStore,
    reconciler: Option<Arc<ServerReconciler>>,
    client: async_nats::Client,
    worker_id: String,
    server_scope: String,
    control_prefix: String,
    ttl: Duration,
    reservations: Mutex<HashMap<String, Instant>>,
}

fn user_token(id: &str) -> String {
    format!("tool-reservation:{id}")
}

fn error(code: &str, message: impl ToString) -> ToolReservationError {
    ToolReservationError {
        code: ToolReservationErrorCode::Other(code.to_owned()),
        message: message.to_string(),
    }
}

fn unknown_or_expired() -> ToolReservationControlReply {
    ToolReservationControlReply::Error(ToolReservationError {
        code: ToolReservationErrorCode::UnknownOrExpired,
        message: "unknown or expired tool reservation".to_owned(),
    })
}

pub(in crate::nats_worker) async fn subscribe(
    runtime: &WorkerRuntime,
    ttl: Duration,
) -> Result<AbortOnDropHandle<()>> {
    let client = runtime.client.clone();
    let reserves = match &runtime.activation_route {
        crate::nats_worker::SessionActivationRoute::ClusterShared => {
            client
                .queue_subscribe(
                    reserve_subject(&runtime.cluster),
                    TOOL_RESERVATION_QUEUE_GROUP.into(),
                )
                .await?
        }
        crate::nats_worker::SessionActivationRoute::WorkerTargeted {
            session_scope,
            worker_id,
        } => {
            client
                .subscribe(targeted_reserve_subject(LocalWorkerTarget::new(
                    session_scope,
                    worker_id,
                )?))
                .await?
        }
    };
    // One wildcard subscription covers opaque per-reservation subjects, including
    // expired IDs: renew must return UnknownOrExpired rather than no responders.
    let control_prefix = client.new_inbox();
    let controls = client.subscribe(format!("{control_prefix}.*")).await?;
    client
        .flush()
        .await
        .context("flush tool reservation subscriptions")?;
    let handler = Arc::new(Handler {
        config: runtime.config.clone(),
        metadata: runtime.session_metadata.clone(),
        reconciler: runtime.server_reconciler.clone(),
        client,
        worker_id: runtime.worker_id.clone(),
        server_scope: runtime.instance_id.as_str().to_owned(),
        control_prefix,
        ttl,
        reservations: Mutex::new(HashMap::new()),
    });
    let shutdown = runtime.shutdown.clone();
    Ok(AbortOnDropHandle::new(tokio::spawn(
        handler.run(reserves, controls, shutdown),
    )))
}

impl Handler {
    async fn run(
        self: Arc<Self>,
        mut reserves: async_nats::Subscriber,
        mut controls: async_nats::Subscriber,
        shutdown: tokio_util::sync::CancellationToken,
    ) {
        let mut tasks = tokio::task::JoinSet::new();
        // Teardown has its own task set, never charged to reserve admission.
        // One sweep at a time coalesces ticks while a child is slow to stop.
        let mut cleanup = tokio::task::JoinSet::new();
        let mut sweep = tokio::time::interval((self.ttl / 3).min(Duration::from_secs(1)));
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                    log_task_result(result, "tool reservation handler task failed");
                }
                Some(result) = cleanup.join_next(), if !cleanup.is_empty() => {
                    log_task_result(result, "tool reservation sweep failed");
                }
                _ = sweep.tick() => {
                    self.expire().await;
                    self.schedule_sweep(&mut cleanup);
                }
                Some(message) = controls.next() => self.control(message).await,
                Some(message) = reserves.next() => {
                    self.admit_reserve(message, &mut tasks).await;
                }
                else => break,
            }
        }
        self.drain(tasks, cleanup).await;
    }

    fn schedule_sweep(&self, cleanup: &mut tokio::task::JoinSet<()>) {
        if !cleanup.is_empty() {
            return;
        }
        if let Some(reconciler) = self.reconciler.clone() {
            cleanup.spawn(async move {
                reconciler.sweep().await;
            });
        }
    }

    async fn admit_reserve(
        self: &Arc<Self>,
        message: async_nats::Message,
        tasks: &mut tokio::task::JoinSet<()>,
    ) {
        if message.reply.is_none() {
            return;
        }
        if tasks.len() >= MAX_PENDING_RESERVES {
            self.reply(
                &message,
                &ReserveReply::Error(error("Busy", "too many pending tool reservations")),
            )
            .await;
            return;
        }
        let handler = Arc::clone(self);
        tasks.spawn(async move {
            handler.reserve(message).await;
        });
    }

    async fn drain(
        &self,
        mut tasks: tokio::task::JoinSet<()>,
        mut cleanup: tokio::task::JoinSet<()>,
    ) {
        // Stop partial claims before releasing their tokens. A canceled
        // claim_users future cannot add a user after this cleanup.
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        let ids: Vec<_> = self
            .reservations
            .lock()
            .await
            .drain()
            .map(|(id, _)| id)
            .collect();
        for id in ids {
            self.free(&id).await;
        }
        // Don't cancel a sweep after it has installed Slot::Stopping: same-name
        // claims must wait until the old child is gone and the slot is removed.
        while cleanup.join_next().await.is_some() {}
        if let Some(reconciler) = &self.reconciler {
            reconciler.sweep().await;
        }
    }

    async fn reply(&self, message: &async_nats::Message, reply: &impl Serialize) {
        if let Some(subject) = &message.reply {
            match serde_json::to_vec(reply) {
                Ok(payload) => {
                    if let Err(error) = self.client.publish(subject.clone(), payload.into()).await {
                        log::warn!("tool reservation reply failed: {error}");
                    }
                }
                Err(error) => log::warn!("tool reservation reply encoding failed: {error}"),
            }
        }
    }

    async fn control(&self, message: async_nats::Message) {
        let control = match serde_json::from_slice::<ToolReservationControl>(&message.payload) {
            Ok(control) => control,
            Err(cause) => {
                self.reply(
                    &message,
                    &ToolReservationControlReply::Error(error("InvalidRequest", cause)),
                )
                .await;
                return;
            }
        };
        let (id, renew) = match control {
            ToolReservationControl::Renew(request) => (request.reservation_id, true),
            ToolReservationControl::Release(request) => (request.reservation_id, false),
        };
        if message.subject.as_str() != format!("{}.{id}", self.control_prefix) {
            // An ID outside this control subject cannot renew or release a
            // different reservation, even on the same worker incarnation.
            let reply = if renew {
                unknown_or_expired()
            } else {
                ToolReservationControlReply::Ok(ToolReservationOk::Ok)
            };
            self.reply(&message, &reply).await;
            return;
        }
        let reply = self.apply_control(&id, renew).await;
        self.reply(&message, &reply).await;
    }

    async fn apply_control(&self, id: &str, renew: bool) -> ToolReservationControlReply {
        let mut reservations = self.reservations.lock().await;
        let live = reservations
            .get(id)
            .is_some_and(|expires| *expires > Instant::now());
        let reply = if renew && live {
            reservations.insert(id.to_owned(), Instant::now() + self.ttl);
            drop(reservations);
            ToolReservationControlReply::Ok(ToolReservationOk::Ok)
        } else {
            let removed = reservations.remove(id).is_some();
            drop(reservations);
            if removed {
                self.free(id).await;
            }
            if renew {
                unknown_or_expired()
            } else {
                ToolReservationControlReply::Ok(ToolReservationOk::Ok)
            }
        };
        reply
    }

    async fn free(&self, id: &str) {
        if let Some(reconciler) = &self.reconciler {
            reconciler.release_users(&user_token(id)).await;
        }
    }

    async fn expire(&self) {
        let now = Instant::now();
        let mut reservations = self.reservations.lock().await;
        let expired: Vec<_> = reservations
            .iter()
            .filter(|(_, expires)| **expires <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            reservations.remove(id);
        }
        drop(reservations);
        for id in expired {
            self.free(&id).await;
        }
    }
}

fn log_task_result(result: std::result::Result<(), tokio::task::JoinError>, message: &str) {
    if let Err(error) = result {
        log::warn!("{message}: {error}");
    }
}

mod reserve;
#[cfg(test)]
mod tests;
