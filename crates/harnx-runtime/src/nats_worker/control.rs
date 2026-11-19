//! Control plane for cancel over NATS.

mod hitl;
mod interaction;
mod surface_action;
mod surface_command;
use hitl::HitlDecisionParams;

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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlCommand {
    /// Cancel the current turn.
    Cancel,
    /// Latency hint that a `Cancel` entry was appended to this session's log.
    Interrupt { cancellation_id: String },
    /// Resolve one durable pending tool approval.
    HitlApprovalDecision {
        tool_call_id: String,
        approved: bool,
        note: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_seq: Option<u64>,
    },
    /// Resolve one durable pending tool interaction.
    InteractionResponse {
        interaction_id: String,
        request_seq: u64,
        response: harnx_core::interaction::InteractionResponse,
    },
    /// Latency hint that a `CompactRequest` entry was appended to this session's log.
    Compact { compaction_id: String },
    /// Apply a frontend's action on a published non-modal A2UI surface (plan
    /// T5.1): validate against the durable generation, run a declared direct
    /// tool target through the operator RPC, append the fenced first-wins
    /// record, and deliver `note` visibility into the running turn.
    SurfaceAction {
        submission: crate::surfaces::SurfaceActionSubmission,
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

#[derive(Clone)]
pub(super) struct SessionControlHandler {
    client: async_nats::Client,
    jetstream: async_nats::jetstream::Context,
    session_id: String,
    lease: Arc<NatsSessionLease>,
    backend: NatsSessionLogBackend,
    abort_signal: crate::utils::AbortSignal,
    hitl_decision_tx: tokio::sync::mpsc::UnboundedSender<AppliedHitlDecision>,
    /// Live queue/turn flag shared with this activation's agent loop.
    surface_actions: crate::surfaces::SurfaceActionQueue,
    /// Operator tool subject for direct tool targets (permissions and
    /// middleware apply there, as for every operator call).
    operator_subject: String,
    /// Per-`(surface_id, publish_seq)` gates serializing surface action
    /// handlers on this worker (review B1): a resend for a generation whose
    /// first attempt is still in flight waits for it instead of running the
    /// target tool again.
    surface_action_gates: SurfaceActionGates,
}

/// The live gate map: one weak in-flight gate per surface generation.
pub(super) type SurfaceActionGateMap =
    std::collections::HashMap<(String, u64), std::sync::Weak<tokio::sync::Mutex<()>>>;

/// In-flight gates serializing surface action handlers, keyed by surface
/// generation. Values are weak so a generation's entry dies with its last
/// handler; dead entries are swept on the next lookup.
pub(super) type SurfaceActionGates = Arc<std::sync::Mutex<SurfaceActionGateMap>>;

impl SessionControlHandler {
    /// Publish a JSON reply to a control command's reply subject. The
    /// decision and surface-action handlers share this. The surface-action
    /// path also publishes early acknowledgements and lease refusals
    /// here, so it proves lease ownership before any reply that claims
    /// the attempt (review N-B).
    pub(super) async fn publish_control_reply<T: serde::Serialize>(
        &self,
        reply: Option<async_nats::Subject>,
        payload: &T,
        what: &str,
    ) {
        let Some(reply) = reply else {
            return;
        };
        let result = async {
            self.client
                .publish(reply, serde_json::to_vec(payload)?.into())
                .await?;
            self.client.flush().await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            log::warn!("failed to reply to {what}: {error:#}");
        }
    }

    /// Whether this worker verifiably owns the session lease (review N-B):
    /// the cheap in-memory flag, then the durable revalidation. An unknown
    /// revalidation state fails closed — a worker that cannot prove
    /// ownership must neither claim a surface action attempt nor append.
    pub(super) async fn holds_session_lease(&self) -> bool {
        self.lease.is_held() && self.lease.revalidate_ownership().await.unwrap_or(false)
    }

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
            surface_actions: ctx.surface_actions.clone(),
            operator_subject: ctx.operator_subject.clone(),
            surface_action_gates: Arc::default(),
        }
    }

    /// The gate serializing handlers for one surface generation (review B1):
    /// a resend whose first attempt is still in flight — including one queued
    /// behind the current handler — joins this gate instead of re-running the
    /// target tool. Entry lifetime follows the handlers: once the last one
    /// drops its gate, the weak map entry is swept on the next lookup.
    pub(super) fn surface_action_gate(
        &self,
        submission: &crate::surfaces::SurfaceActionSubmission,
    ) -> Arc<tokio::sync::Mutex<()>> {
        let key = (submission.surface_id.clone(), submission.publish_seq);
        // The gate map is a cache of in-flight handlers: a poisoned lock
        // only follows a panic in this same critical section, and the
        // contained state is recoverable by design, so recovering it beats
        // failing the control plane.
        let mut gates = self
            .surface_action_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        surface_action::live_gate(&mut gates, key)
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
                request_seq,
            } => {
                let params = HitlDecisionParams::new(tool_call_id, approved, note, request_seq);
                self.apply_hitl_decision(params, reply).await
            }
            ControlCommand::InteractionResponse {
                interaction_id,
                request_seq,
                response,
            } => {
                self.apply_interaction_response(&interaction_id, request_seq, &response, reply)
                    .await;
            }
            ControlCommand::Compact { .. } => {
                self.acknowledge(reply).await;
            }
            ControlCommand::SurfaceAction { submission } => {
                self.handle_surface_action_command(submission, reply).await;
            }
        }
    }

    async fn confirm_logged_interrupt(&self, reply: Option<async_nats::Subject>) {
        let Ok(Some((_, harnx_core::session::SessionLogEntry::Cancel { .. }))) =
            self.backend.last_entry_async().await
        else {
            return;
        };
        self.abort_signal.set_ctrlc();
        self.acknowledge(reply).await;
    }

    async fn cancel(&self, reply: Option<async_nats::Subject>) {
        if self.append_cancel().await {
            self.acknowledge(reply).await;
        }
        self.abort_signal.set_ctrlc();
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
/// Format: `sessions.{session_id}.control`
pub fn control_subject(session_id: &str) -> String {
    format!("sessions.{session_id}.control")
}

/// Publish a control command to a session's control subject.
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

#[cfg(test)]
#[path = "control_hitl_tests.rs"]
mod hitl_tests;

#[cfg(test)]
#[path = "control_surface_tests.rs"]
mod surface_tests;
