//! Request/reply bridge for interactive tool approval across NATS workers.
//!
//! An attached frontend owns a session-scoped reply subject. Direct and queued
//! activations carry that subject to the worker, which sends `PreToolUse` `ask`
//! decisions there instead of reading from the worker process's non-interactive
//! stdin.
//!
//! The reply subject is lifecycle routing, not an authentication boundary.
//! Confirmation transport inherits the NATS account's trust boundary, just
//! like the other request/reply services: deployments with mutually untrusted
//! clients must isolate them with NATS accounts and subject permissions.

use crate::tool::{ConfirmToolUseFn, ToolCall, ToolUseConfirmation};
use crate::utils::{wait_abort_signal, AbortSignal};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

pub const TOOL_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolConfirmationRequest {
    pub session_id: String,
    pub tool_call_id: Option<String>,
    pub tool_name: String,
    pub arguments: Value,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct ToolConfirmationResponse {
    approved: bool,
}

pub type ToolConfirmationFuture = Pin<Box<dyn Future<Output = bool> + Send>>;
pub type ToolConfirmationHandler =
    dyn Fn(ToolConfirmationRequest) -> ToolConfirmationFuture + Send + Sync;

/// Frontend-owned confirmation route. Keep this alive while the frontend is
/// attached to the session so queued continuation turns can reuse its subject.
pub struct ToolConfirmationRoute {
    subject: String,
    _responder: ToolConfirmationResponder,
}

impl ToolConfirmationRoute {
    pub(crate) async fn start(
        client: async_nats::Client,
        handler: Arc<ToolConfirmationHandler>,
    ) -> Result<Self> {
        let subject = client.new_inbox();
        let responder = ToolConfirmationResponder::start(client, subject.clone(), handler).await?;
        Ok(Self {
            subject,
            _responder: responder,
        })
    }

    pub(crate) fn subject(&self) -> &str {
        &self.subject
    }

    /// Begin closing this route. Safe to call while other owners still retain
    /// the route, as happens when a frontend switches sessions during a turn.
    pub fn shutdown(&self) {
        self._responder.shutdown();
    }

    /// Close the route and wait until NATS has observed the unsubscribe.
    pub async fn close(&self) {
        self._responder.close().await;
    }
}

/// A subscribed frontend confirmation service. Dropping it unsubscribes and
/// denies any request already delivered before the unsubscribe reached NATS.
struct ToolConfirmationResponder {
    shutdown: parking_lot::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl ToolConfirmationResponder {
    async fn start(
        client: async_nats::Client,
        subject: String,
        handler: Arc<ToolConfirmationHandler>,
    ) -> Result<Self> {
        let mut subscriber = client
            .subscribe(subject)
            .await
            .context("subscribe to tool confirmations")?;
        client
            .flush()
            .await
            .context("flush tool confirmation subscription")?;
        let (shutdown, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                let message = tokio::select! {
                    _ = &mut shutdown_rx => break,
                    message = subscriber.next() => {
                        let Some(message) = message else { break };
                        message
                    }
                };
                let request = serde_json::from_slice(&message.payload);
                let decision = async {
                    match request {
                        Ok(request) => handler(request).await,
                        Err(error) => {
                            log::warn!("invalid tool confirmation request: {error}");
                            false
                        }
                    }
                };
                tokio::pin!(decision);
                let (approved, shutting_down) = tokio::select! {
                    approved = &mut decision => (approved, false),
                    _ = &mut shutdown_rx => (false, true),
                };
                publish_confirmation_response(&client, message, approved).await;
                if shutting_down {
                    break;
                }
            }

            // UNSUB followed by flush establishes a boundary: requests the
            // server routed before UNSUB are now buffered locally, while later
            // requests get a no-responders error at the worker.
            if let Err(error) = subscriber.unsubscribe().await {
                log::debug!("failed to unsubscribe tool confirmation responder: {error}");
            }
            if let Err(error) = client.flush().await {
                log::debug!("failed to flush tool confirmation unsubscribe: {error}");
            }
            while let Some(message) = subscriber.next().await {
                publish_confirmation_response(&client, message, false).await;
            }
        });
        Ok(Self {
            shutdown: parking_lot::Mutex::new(Some(shutdown)),
            task: tokio::sync::Mutex::new(Some(task)),
        })
    }

    fn shutdown(&self) {
        if let Some(shutdown) = self.shutdown.lock().take() {
            let _ = shutdown.send(());
        }
    }

    async fn close(&self) {
        self.shutdown();
        if let Some(task) = self.task.lock().await.take() {
            let _ = task.await;
        }
    }
}

impl Drop for ToolConfirmationResponder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn publish_confirmation_response(
    client: &async_nats::Client,
    message: async_nats::Message,
    approved: bool,
) {
    let Some(reply) = message.reply else {
        log::warn!("tool confirmation request had no reply subject");
        return;
    };
    let payload = match serde_json::to_vec(&ToolConfirmationResponse { approved }) {
        Ok(payload) => payload,
        Err(error) => {
            log::warn!("failed to encode tool confirmation response: {error}");
            return;
        }
    };
    if let Err(error) = client.publish(reply, payload.into()).await {
        log::warn!("failed to publish tool confirmation response: {error}");
    }
}

/// Build the worker-side synchronous callback expected by tool evaluation.
/// The callback blocks only the current tool-evaluation task while the worker's
/// other runtime tasks (lease renewal, cancellation, and NATS I/O) continue.
pub(crate) fn nats_confirm_tool_use(
    client: async_nats::Client,
    subject: String,
    session_id: String,
    abort_signal: AbortSignal,
) -> Arc<ConfirmToolUseFn> {
    Arc::new(
        move |call: &ToolCall, arguments: &Value, reason: Option<&str>| {
            let request = ToolConfirmationRequest {
                session_id: session_id.clone(),
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                arguments: arguments.clone(),
                reason: reason.map(str::to_string),
            };
            let payload = match serde_json::to_vec(&request) {
                Ok(payload) => payload,
                Err(error) => {
                    log::warn!("failed to encode tool confirmation request: {error}");
                    return denied(reason);
                }
            };
            let nats_request = async_nats::Request::new()
                .payload(payload.into())
                .timeout(Some(TOOL_CONFIRMATION_TIMEOUT));
            let abort_signal = abort_signal.clone();
            let response = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    tokio::select! {
                        _ = wait_abort_signal(&abort_signal) => None,
                        response = client.send_request(subject.clone(), nats_request) => {
                            Some(response)
                        }
                    }
                })
            });
            let response = match response {
                None => {
                    log::info!("tool confirmation cancelled; denying call");
                    return denied(reason);
                }
                Some(Ok(response)) => response,
                Some(Err(error)) => {
                    log::warn!("tool confirmation unavailable; denying call: {error}");
                    return denied(reason);
                }
            };
            match serde_json::from_slice::<ToolConfirmationResponse>(&response.payload) {
                Ok(ToolConfirmationResponse { approved: true }) => ToolUseConfirmation::Approve,
                Ok(ToolConfirmationResponse { approved: false }) => denied(reason),
                Err(error) => {
                    log::warn!("invalid tool confirmation response; denying call: {error}");
                    denied(reason)
                }
            }
        },
    )
}

fn denied(reason: Option<&str>) -> ToolUseConfirmation {
    ToolUseConfirmation::Deny {
        reason: reason.map(str::to_string),
    }
}
