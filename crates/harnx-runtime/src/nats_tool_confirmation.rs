//! Request/reply bridge for interactive tool approval across NATS workers.
//!
//! The frontend that activates a turn owns a unique reply subject. The worker
//! sends `PreToolUse` `ask` decisions to that subject and waits for the
//! frontend's response instead of trying to read from the worker process's
//! non-interactive stdin.
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

/// A subscribed frontend confirmation service. Dropping it stops accepting
/// requests so stale turn clients cannot consume approval prompts.
pub(crate) struct ToolConfirmationResponder {
    task: tokio::task::JoinHandle<()>,
}

impl ToolConfirmationResponder {
    pub(crate) async fn start(
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
        let task = tokio::spawn(async move {
            while let Some(message) = subscriber.next().await {
                let approved = match serde_json::from_slice(&message.payload) {
                    Ok(request) => handler(request).await,
                    Err(error) => {
                        log::warn!("invalid tool confirmation request: {error}");
                        false
                    }
                };
                let Some(reply) = message.reply else {
                    log::warn!("tool confirmation request had no reply subject");
                    continue;
                };
                let payload = match serde_json::to_vec(&ToolConfirmationResponse { approved }) {
                    Ok(payload) => payload,
                    Err(error) => {
                        log::warn!("failed to encode tool confirmation response: {error}");
                        continue;
                    }
                };
                if let Err(error) = client.publish(reply, payload.into()).await {
                    log::warn!("failed to publish tool confirmation response: {error}");
                }
            }
        });
        Ok(Self { task })
    }
}

impl Drop for ToolConfirmationResponder {
    fn drop(&mut self) {
        self.task.abort();
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
