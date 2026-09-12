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
    /// Generation-scoped latency hint. Durable KV state is authoritative.
    CancelExecution {
        execution_id: String,
        cancellation_id: String,
    },
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
    execution: super::execution_control::WorkerExecution,
    hitl_decision_tx: tokio::sync::mpsc::UnboundedSender<AppliedHitlDecision>,
}

impl SessionControlHandler {
    pub(super) fn new(
        ctx: &super::daemon_runtime::ControlListenerCtx<'_>,
        hitl_decision_tx: tokio::sync::mpsc::UnboundedSender<AppliedHitlDecision>,
    ) -> Self {
        Self {
            execution: ctx.execution.clone(),
            client: ctx.client.clone(),
            jetstream: ctx.jetstream.clone(),
            session_id: ctx.session_id.to_string(),
            lease: Arc::clone(ctx.lease),
            backend: ctx.backend.clone(),
            abort_signal: ctx.abort_signal.clone(),
            hitl_decision_tx,
        }
    }

    pub(super) async fn listen(
        self,
        mut subscriber: async_nats::Subscriber,
        mut watch: harnx_nats_common::recovery::KvUpdates,
    ) {
        use futures_util::StreamExt;
        loop {
            if !self.abort_signal.aborted()
                && self
                    .execution
                    .store
                    .check_ancestors(&self.execution.reference)
                    .await
                    .is_err()
            {
                self.cancel(None).await;
            }
            tokio::select! {
                update = watch.next() => {
                    if !matches!(update, Some(Ok(_))) {
                        self.abort_signal.set_ctrlc();
                        let _ = self.execution.store.cancel_operation(&self.execution.reference, None, false).await;
                        return;
                    }
                }
                message = subscriber.next() => {
                    let Some(message) = message else { self.abort_signal.set_ctrlc(); return; };
                    match ControlCommand::from_bytes(&message.payload) {
                        Ok(ControlCommand::CancelExecution { execution_id, cancellation_id }) if execution_id == self.execution.reference.execution_id => {
                            let _ = self.execution.store.cancel_operation(&self.execution.reference, Some(&cancellation_id), false).await;
                            self.cancel(message.reply).await;
                        }
                        Ok(ControlCommand::HitlApprovalDecision { tool_call_id, approved, note }) => {
                            self.apply_hitl_decision(tool_call_id, approved, note, message.reply).await;
                        }
                        Ok(ControlCommand::Cancel) => self.cancel(message.reply).await,
                        Ok(ControlCommand::CancelExecution { .. }) => {}
                        Err(error) => log::debug!("invalid control command payload, ignoring: {error}"),
                    }
                }
            }
        }
    }

    async fn cancel(&self, reply: Option<async_nats::Subject>) {
        self.abort_signal.set_ctrlc();
        let _ = self
            .execution
            .store
            .cancel_operation(&self.execution.reference, None, false)
            .await;
        let _ = self
            .execution
            .store
            .quiesce(&self.execution.reference, &self.execution.owner)
            .await;
        let durable = self.append_cancel();
        if durable {
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
        let event_sink = crate::nats_event_sink::NatsEventSink::new(
            self.client.clone(),
            self.jetstream.clone(),
            self.session_id.clone(),
        )
        .await;
        event_sink.publish_session_updated();
        let _ = self.hitl_decision_tx.send(AppliedHitlDecision {
            tool_call_id,
            approved,
            note,
        });
        self.acknowledge(reply).await;
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
    let command = durable_command(client, session_id, command).await?;
    let Some(command) = command else {
        return Ok(());
    };
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
    let command = durable_command(client, session_id, command).await?;
    let Some(command) = command else {
        return Ok(());
    };
    let subject = control_subject(session_id);
    let payload = command.to_bytes()?;
    tokio::time::timeout(timeout, client.request(subject, payload.into()))
        .await
        .context("timed out waiting for session control acknowledgement")?
        .context("request session control acknowledgement")?;
    Ok(())
}

async fn durable_command(
    client: &async_nats::Client,
    session_id: &str,
    command: &ControlCommand,
) -> Result<Option<ControlCommand>> {
    if !matches!(command, ControlCommand::Cancel) {
        return Ok(Some(command.clone()));
    }
    let js = async_nats::jetstream::new(client.clone());
    // Legacy callers do not carry cluster configuration. Opening an existing
    // bucket must preserve its replica count rather than applying a local default.
    let store = match js.get_key_value(harnx_execution_control::BUCKET).await {
        Ok(bucket) => harnx_execution_control::ExecutionStore::from_store(bucket),
        Err(error) if crate::nats_admin::kv_bucket_missing(&error) => {
            harnx_execution_control::ExecutionStore::ensure(&js, 1).await?
        }
        Err(error) => return Err(error.into()),
    };
    let Some(operation) =
        crate::nats_session::cancellation::resolve_pending_execution(&store, &js, session_id)
            .await?
    else {
        return Ok(None);
    };
    let operation = store
        .cancel_operation(&operation.reference, None, false)
        .await?;
    let cancel = operation.cancellation.context("cancellation missing")?;
    Ok(Some(ControlCommand::CancelExecution {
        execution_id: operation.reference.execution_id,
        cancellation_id: cancel.cancellation_id,
    }))
}
