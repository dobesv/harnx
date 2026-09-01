//! Control plane for cancel over NATS.

use super::backend::NatsSessionLogBackend;
use super::daemon::should_append_control_log_entry;
use crate::nats_lease::NatsSessionLease;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// P2.4 Control plane: cancel over NATS
// ---------------------------------------------------------------------------

/// Control command sent over the control subject (`sessions.{id}.control`).
///
/// Clients publish these commands to interact with an active session without
/// going through the durable activation workflow. Workers optionally
/// acknowledge request/reply delivery after durable state lands in the session
/// log; plain publish remains supported for callers that do not need
/// confirmation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlCommand {
    /// Cancel the current turn.
    ///
    /// Worker-originated: carries the fence token for tombstone.
    /// The worker appends a Cancel entry BEFORE firing the AbortSignal.
    Cancel,
}

impl ControlCommand {
    /// Serialize the command to JSON bytes for NATS publish.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Deserialize from JSON bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        Ok(serde_json::from_slice(data)?)
    }
}

pub(super) struct SessionControlHandler {
    client: async_nats::Client,
    lease: Arc<NatsSessionLease>,
    backend: NatsSessionLogBackend,
    abort_signal: crate::utils::AbortSignal,
}

impl SessionControlHandler {
    pub(super) fn new(
        client: &async_nats::Client,
        lease: &Arc<NatsSessionLease>,
        backend: &NatsSessionLogBackend,
        abort_signal: &crate::utils::AbortSignal,
    ) -> Self {
        Self {
            client: client.clone(),
            lease: Arc::clone(lease),
            backend: backend.clone(),
            abort_signal: abort_signal.clone(),
        }
    }

    pub(super) async fn listen(self, mut subscriber: async_nats::Subscriber) {
        use futures_util::StreamExt;
        while let Some(message) = subscriber.next().await {
            let Ok(ControlCommand::Cancel) = ControlCommand::from_bytes(&message.payload) else {
                log::debug!("invalid control command payload, ignoring");
                continue;
            };
            self.cancel(message.reply).await;
        }
    }

    async fn cancel(&self, reply: Option<async_nats::Subject>) {
        let durable = self.append_cancel();
        if durable {
            self.acknowledge(reply).await;
        }
        self.abort_signal.set_ctrlc();
    }

    fn append_cancel(&self) -> bool {
        if !should_append_control_log_entry(&self.lease) {
            return false;
        }
        let entry = harnx_core::session::SessionLogEntry::Cancel {
            fence_token: self.lease.fence_token(),
        };
        if let Err(error) = self.backend.append_event_blocking(&entry) {
            log::warn!("failed to append Cancel entry: {error}");
            return false;
        }
        true
    }

    async fn acknowledge(&self, reply: Option<async_nats::Subject>) {
        let Some(reply) = reply else {
            return;
        };
        if let Err(error) = self.client.publish(reply, bytes::Bytes::new()).await {
            log::warn!("failed to publish session cancel acknowledgement: {error}");
            return;
        }
        if let Err(error) = self.client.flush().await {
            log::warn!("failed to flush session cancel acknowledgement: {error}");
        }
    }
}

/// Control subject pattern for a session.
///
/// Format: `sessions.{session_id}.control`
pub fn control_subject(session_id: &str) -> String {
    format!("sessions.{session_id}.control")
}

/// Publish a control command to a session's control subject.
///
/// This is the client-side helper for driving control commands. Workers
/// subscribe to this subject and handle commands when holding the lease.
pub async fn publish_control_command(
    client: &async_nats::Client,
    session_id: &str,
    command: &ControlCommand,
) -> Result<()> {
    let subject = control_subject(session_id);
    let payload = command.to_bytes()?;
    client
        .publish(subject, payload.into())
        .await
        .context("publish control command")?;
    client
        .flush()
        .await
        .context("flush control command publish")?;
    Ok(())
}

/// Send a control command and wait for the lease holder to confirm that its
/// durable control entry was written.
///
/// A missing subscriber is reported by the caller-provided timeout. This lets
/// recovery code re-activate an orphaned session and retry without mistaking a
/// successful NATS publish for a handled cancellation.
pub async fn request_control_command(
    client: &async_nats::Client,
    session_id: &str,
    command: &ControlCommand,
    timeout: std::time::Duration,
) -> Result<()> {
    let subject = control_subject(session_id);
    let payload = command.to_bytes()?;
    tokio::time::timeout(timeout, client.request(subject, payload.into()))
        .await
        .context("timed out waiting for session control acknowledgement")?
        .context("request session control acknowledgement")?;
    Ok(())
}
