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
    /// Latency hint that a `Cancel` entry was appended to this session's log.
    /// The log is authoritative; the listener re-reads the tail on receipt.
    Interrupt { cancellation_id: String },
    /// Resolve one durable pending tool approval.
    HitlApprovalDecision {
        tool_call_id: String,
        approved: bool,
        note: Option<String>,
    },
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AppliedHitlDecision {
    pub tool_call_id: String,
    pub approved: bool,
    pub note: Option<String>,
}
pub(super) struct SessionControlHandler {
    client: async_nats::Client,
    jetstream: async_nats::jetstream::Context,
    session_id: String,
    lease: Arc<NatsSessionLease>,
    backend: NatsSessionLogBackend,
    abort_signal: crate::utils::AbortSignal,
    hitl_decision_tx: tokio::sync::mpsc::UnboundedSender<AppliedHitlDecision>,
}

impl SessionControlHandler {
    pub(super) fn new(
        ctx: &super::daemon_runtime::ControlListenerCtx<'_>,
        hitl_decision_tx: tokio::sync::mpsc::UnboundedSender<AppliedHitlDecision>,
    ) -> Self {
        Self {
            client: ctx.client.clone(),
            jetstream: ctx.jetstream.clone(),
            session_id: ctx.session_id.to_string(),
            lease: Arc::clone(ctx.lease),
            backend: ctx.backend.clone(),
            abort_signal: ctx.abort_signal.clone(),
            hitl_decision_tx,
        }
    }

    pub(super) async fn listen(self, mut subscriber: async_nats::Subscriber) {
        use futures_util::StreamExt;
        while let Some(message) = subscriber.next().await {
            match ControlCommand::from_bytes(&message.payload) {
                Ok(command) => self.apply(command, message.reply).await,
                Err(error) => {
                    log::debug!("invalid control command payload, ignoring: {error}")
                }
            }
        }
        self.abort_signal.set_ctrlc();
    }

    async fn apply(&self, command: ControlCommand, reply: Option<async_nats::Subject>) {
        match command {
            ControlCommand::Interrupt { .. } => self.confirm_logged_interrupt(reply).await,
            ControlCommand::Cancel => self.cancel(reply).await,
            ControlCommand::HitlApprovalDecision {
                tool_call_id,
                approved,
                note,
            } => {
                self.apply_hitl_decision(tool_call_id, approved, note, reply)
                    .await
            }
        }
    }

    /// An `Interrupt` command is only a hint that a `Cancel` was appended. The
    /// log decides: abort (and acknowledge) only once it carries one for the
    /// turn in progress.
    async fn confirm_logged_interrupt(&self, reply: Option<async_nats::Subject>) {
        let Ok(entries) = self.backend.load_events_latest_async().await else {
            return;
        };
        if harnx_core::session_reconstruct::current_turn_is_cancelled(&entries) {
            self.abort_signal.set_ctrlc();
            self.acknowledge(reply).await;
        }
    }

    /// The legacy client cancel: no `Cancel` is in the log yet, so this worker
    /// writes one itself before aborting, exactly as a frontend's
    /// `interrupt_session` would have.
    async fn cancel(&self, reply: Option<async_nats::Subject>) {
        if self.append_cancel().await {
            self.acknowledge(reply).await;
        }
        self.abort_signal.set_ctrlc();
    }

    async fn apply_hitl_decision(
        &self,
        tool_call_id: String,
        approved: bool,
        note: Option<String>,
        reply: Option<async_nats::Subject>,
    ) {
        if !should_append_control_log_entry(&self.lease) {
            return;
        }
        let entries = match self.backend.load_events_latest_async().await {
            Ok(entries) => entries,
            Err(error) => {
                log::warn!("failed to load pending HITL approvals: {error:#}");
                return;
            }
        };
        let pending = match super::agent_loop::derive_pending_hitl_approvals(&entries) {
            Ok(pending) => pending,
            Err(error) => {
                log::warn!("failed to derive pending HITL approvals: {error:#}");
                return;
            }
        };
        if !pending
            .iter()
            .any(|approval| approval.tool_call_id == tool_call_id)
        {
            return;
        }
        if !self.lease.is_held() {
            return;
        }
        let entry = harnx_core::session::SessionLogEntry::HitlApprovalDecision {
            tool_call_id: tool_call_id.clone(),
            approved,
            note: note.clone(),
            fence_token: self.lease.fence_token(),
        };
        let expected_last_sequence = entries.last().map_or(0, |(seq, _)| *seq);
        let sink = super::backend::FencedSessionLogSink::new(
            self.backend.clone(),
            Arc::clone(&self.lease),
        );
        match sink
            .append_hitl_event_cas(&entry, expected_last_sequence)
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => {
                match self.backend.load_events_latest_async().await {
                    Ok(reloaded) => {
                        if let Err(error) =
                            super::agent_loop::derive_pending_hitl_approvals(&reloaded)
                        {
                            log::warn!(
                                "failed to re-derive HITL approvals after decision CAS loss: {error:#}"
                            );
                        }
                    }
                    Err(error) => {
                        log::warn!(
                            "failed to reload HITL approvals after decision CAS loss: {error:#}"
                        );
                    }
                }
                return;
            }
            Err(error) => {
                log::warn!("failed to append HITL approval decision: {error:#}");
                return;
            }
        }
        self.publish_updated().await;
        let _ = self.hitl_decision_tx.send(AppliedHitlDecision {
            tool_call_id,
            approved,
            note,
        });
        self.acknowledge(reply).await;
    }
    async fn publish_updated(&self) {
        let event_sink = crate::nats_event_sink::NatsEventSink::new(
            self.client.clone(),
            self.jetstream.clone(),
            self.session_id.clone(),
        )
        .await;
        event_sink.publish_session_updated();
    }

    async fn append_cancel(&self) -> bool {
        if !should_append_control_log_entry(&self.lease) {
            return false;
        }
        let entries = match self.backend.load_events_latest_async().await {
            Ok(entries) => entries,
            Err(error) => {
                log::warn!("failed to read the log before appending Cancel: {error:#}");
                return false;
            }
        };
        if harnx_core::session_reconstruct::current_turn_is_cancelled(&entries) {
            return true;
        }
        let entry = harnx_core::session::SessionLogEntry::Cancel {
            fence_token: self.lease.fence_token(),
            cancellation_id: Some(uuid::Uuid::now_v7().to_string()),
            requested_by: Some("client:legacy".into()),
            timestamp: Some(chrono::Utc::now()),
        };
        let expected_tail = entries.last().map_or(0, |(seq, _)| *seq);
        if let Err(error) = self
            .backend
            .append_event_fenced_with_lease(&entry, &self.lease, expected_tail)
            .await
        {
            if error.is::<super::backend::TurnInterrupted>() {
                return true;
            }
            log::warn!("failed to append Cancel entry: {error:#}");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_command_round_trips() {
        let command = ControlCommand::Interrupt {
            cancellation_id: "c-1".into(),
        };
        let back = ControlCommand::from_bytes(&command.to_bytes().unwrap()).unwrap();
        assert!(
            matches!(back, ControlCommand::Interrupt { cancellation_id } if cancellation_id == "c-1")
        );
    }
}
