//! Advisories only wake durable reads; unready requests never reach the client.

use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::interactions::{derive_pending_interactions, PendingInteraction};
use harnx_runtime::nats_event_sink::SessionEventStream;
use harnx_runtime::NatsSession;
use tokio::sync::{watch, Mutex};

use super::{decision_error, decision_response, permission_request, report, Connections};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
type Identity = (String, u64);

pub(super) struct Observer {
    pub session: NatsSession,
    pub config: GlobalConfig,
    pub cluster: String,
    pub connections: Connections,
    pub gate: Arc<Mutex<()>>,
    pub cancel: watch::Receiver<u64>,
}

impl Observer {
    pub async fn run(mut self) {
        let mut suppressed = HashSet::new();
        let mut last_error = None;
        loop {
            match self.observe_connection(&mut suppressed).await {
                Err(error) => {
                    let error = format!("cannot read pending requests: {error:#}");
                    if last_error.as_ref() != Some(&error) {
                        self.report(error.clone()).await;
                        last_error = Some(error);
                    }
                }
                Ok(()) => last_error = None,
            }
            tokio::time::sleep(REFRESH_INTERVAL).await;
        }
    }

    async fn observe_connection(&mut self, suppressed: &mut HashSet<Identity>) -> Result<()> {
        let mut stream =
            super::attach_stream(&self.config, &self.cluster, self.session.storage_key()).await?;
        loop {
            self.process_head(&mut stream, suppressed).await?;
            // SuspensionReady and decisions need no advisory. Poll even when the worker is quiet.
            if let Ok(None) = tokio::time::timeout(REFRESH_INTERVAL, stream.next()).await {
                return Ok(());
            }
            stream.refresh_history().await?;
        }
    }

    async fn process_head(
        &mut self,
        stream: &mut SessionEventStream,
        suppressed: &mut HashSet<Identity>,
    ) -> Result<()> {
        if self.cancel.has_changed()? {
            self.cancel.borrow_and_update();
            abandon_pending(stream, suppressed)?;
            return Ok(());
        }
        let Some(pending) = pending_head(stream, suppressed)? else {
            return Ok(());
        };
        let gate = Arc::clone(&self.gate);
        // Legacy HITL callbacks use this same gate. Recheck the manifest after waiting for it.
        let _guard = tokio::select! {
            biased;
            _ = self.cancel.changed() => {
                abandon_pending(stream, suppressed)?;
                return Ok(());
            },
            guard = gate.lock() => guard,
        };
        stream.refresh_history().await?;
        if pending_head(stream, suppressed)?.as_ref() != Some(&pending) {
            return Ok(());
        }
        let Some(connection) = self.connections.read().await.clone() else {
            return Ok(());
        };
        if connection.is_incoming_closed() {
            return Ok(());
        }
        let request = permission_request(self.session.session_id(), &pending);
        let outcome = tokio::select! {
            biased;
            _ = self.cancel.changed() => {
                abandon_pending(stream, suppressed)?;
                return Ok(());
            },
            reply = connection.send_request(request).block_task() => reply,
        };
        match self.accept_reply(&pending, outcome).await {
            Ok(()) => {
                suppressed.insert(identity(&pending));
            }
            Err(error) => {
                self.report(format!(
                    "response rejected for '{}' at request sequence {}: {error:#}",
                    pending.interaction_id, pending.request_seq
                ))
                .await;
                // Failed acceptance leaves the manifest head pending. Retry after a backoff,
                // including after worker failover or a replacement client connection.
                tokio::time::sleep(REFRESH_INTERVAL).await;
            }
        }
        Ok(())
    }

    async fn report(&self, message: String) {
        // The connection used for the permission RPC may have been replaced while it ran.
        if let Some(connection) = self.connections.read().await.clone() {
            report(&connection, self.session.session_id(), message);
        }
    }

    async fn accept_reply(
        &self,
        pending: &PendingInteraction,
        reply: agent_client_protocol::Result<
            agent_client_protocol::v1::schema::RequestPermissionResponse,
        >,
    ) -> Result<()> {
        // Only an explicit Cancelled outcome is a denial. Transport failure isn't consent
        // to either action: leave the interaction pending for the reconnected client.
        let outcome = reply.context("permission request failed; interaction remains pending")?;
        let response = decision_response(pending, outcome.outcome).map_err(anyhow::Error::msg)?;
        let outcome = self
            .session
            .decide_interaction(&pending.interaction_id, pending.request_seq, &response)
            .await
            .context("durable decision failed")?;
        if let Some(error) = decision_error(outcome) {
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

fn identity(pending: &PendingInteraction) -> Identity {
    (pending.interaction_id.clone(), pending.request_seq)
}

fn pending_head(
    stream: &SessionEventStream,
    suppressed: &mut HashSet<Identity>,
) -> Result<Option<PendingInteraction>> {
    let pending = derive_pending_interactions(stream.history())?;
    // Retain only unsettled identities, not every interaction from the session's lifetime.
    suppressed.retain(|(id, seq)| {
        pending
            .iter()
            .any(|item| &item.interaction_id == id && item.request_seq == *seq)
    });
    // Never skip a cancelled-but-still-pending head to show a later manifest item (R9).
    Ok(pending
        .first()
        .filter(|head| !suppressed.contains(&identity(head)))
        .cloned())
}

fn abandon_pending(stream: &SessionEventStream, suppressed: &mut HashSet<Identity>) -> Result<()> {
    suppressed.extend(
        derive_pending_interactions(stream.history())?
            .iter()
            .map(identity),
    );
    Ok(())
}
