//! Real JetStream log, lease and T2.2 acceptance; only the control transport is a test responder.
use std::{sync::Arc, time::Duration};

use agent_client_protocol::schema::v1::{
    PermissionOptionId, RequestPermissionOutcome, RequestPermissionRequest,
    SelectedPermissionOutcome, SessionId,
};
use anyhow::{Context, Result};
use futures::StreamExt;
use harnx_core::{interaction::InteractionResponse, session::SessionLogEntry, tool::ToolCall};
use harnx_runtime::{
    interactions::accept_interaction_response,
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session_log::NatsSessionLog,
    nats_worker::{control_subject, ControlCommand, NatsSessionLogBackend},
};
use serde_json::json;
use tokio_util::task::AbortOnDropHandle;

use super::super::support::*;

pub(super) const QUIET: Duration = Duration::from_millis(1200);

pub(super) struct Fixture {
    pub agent: Arc<harnx_acp_server::HarnxAgent>,
    pub client: TestClient,
    pub log: NatsSessionLog,
    pub lease: Arc<NatsSessionLease>,
    pub backend: NatsSessionLogBackend,
    pub session_id: SessionId,
    pub round: u64,
    pub force_outcomes:
        tokio::sync::mpsc::UnboundedSender<harnx_runtime::interactions::InteractionDecisionOutcome>,
    _control: AbortOnDropHandle<()>,
    _server: NatsServerHandle,
}

impl Fixture {
    pub async fn spawn() -> Result<Option<Self>> {
        let Some(server) = spawn_nats_server().await? else {
            return Ok(None);
        };
        let config = test_config(&server.url);
        let agent = Arc::new(harnx_acp_server::HarnxAgent::with_nats_config(
            AGENT_NAME.into(),
            harnx_acp_server::NatsAgentConfig {
                runtime_config: config.clone(),
                cluster: CLUSTER.into(),
                activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
                session_initializer: harnx_runtime::SessionInitializer::named(
                    AGENT_NAME,
                    Default::default(),
                ),
            },
        ));
        let client = attach_test_client(Arc::clone(&agent), PermissionReply::Manual).await?;
        let session_id = initialize_and_create_session(&agent).await?;
        let snapshot = config.read().clone();
        let js = snapshot.nats_jetstream(CLUSTER).await?;
        let nats = snapshot.nats_client(CLUSTER).await?;
        let key =
            harnx_core::session_identity::session_key(Some(AGENT_NAME), session_id.0.as_ref());
        let log = NatsSessionLog::new_with_replicas(js.clone(), key.clone(), 1);
        let lease = Arc::new(
            NatsSessionLease::acquire(NatsLeaseAcquireParams {
                jetstream: js.clone(),
                session_id: &key,
                worker_id: "acp-interaction-test".into(),
                generation: 1,
                config: NatsLeaseConfig::default(),
                session_metadata: None,
            })
            .await?
            .context("acquire fixture lease")?,
        );
        let backend = NatsSessionLogBackend::new(js, key.to_string(), 1);
        let mut commands = nats.subscribe(control_subject(&key)).await?;
        nats.flush().await?;
        let held = lease.clone();
        let target = backend.clone();
        let (force_outcomes, mut forced) = tokio::sync::mpsc::unbounded_channel();
        let control = tokio::spawn(async move {
            while let Some(message) = commands.next().await {
                let ControlCommand::InteractionResponse {
                    interaction_id,
                    request_seq,
                    response,
                } = ControlCommand::from_bytes(&message.payload).unwrap()
                else {
                    continue;
                };
                let outcome = match forced.try_recv() {
                    Ok(outcome) => outcome,
                    Err(_) => accept_interaction_response(
                        &target,
                        &held,
                        &interaction_id,
                        request_seq,
                        &response,
                    )
                    .await
                    .unwrap(),
                };
                nats.publish(
                    message.reply.unwrap(),
                    serde_json::to_vec(&outcome).unwrap().into(),
                )
                .await
                .unwrap();
            }
        });
        let round = log
            .append_event_async(&SessionLogEntry::ToolCalls {
                text: String::new(),
                thought: None,
                calls: vec![ToolCall::new(
                    "counter_run".into(),
                    json!({}),
                    Some("call-1".into()),
                    None,
                )],
                timestamp: None,
                fence_token: Some(lease.fence_token()),
            })
            .await?;
        Ok(Some(Self {
            agent,
            client,
            log,
            lease,
            backend,
            session_id,
            round,
            force_outcomes,
            _control: AbortOnDropHandle::new(control),
            _server: server,
        }))
    }

    pub async fn spawn_pending(id: &str) -> Result<Option<(Self, u64)>> {
        let Some(fixture) = Self::spawn().await? else {
            return Ok(None);
        };
        let seq = fixture.request(id, "confirm").await?;
        fixture.ready(&[id]).await?;
        Ok(Some((fixture, seq)))
    }

    pub async fn restart_round(&mut self) -> Result<()> {
        let entry = self
            .log
            .load_events_latest_async()
            .await?
            .into_iter()
            .find(|(seq, _)| *seq == self.round)
            .context("round entry")?
            .1;
        self.round = self.log.append_event_async(&entry).await?;
        Ok(())
    }

    pub async fn request(&self, id: &str, kind: &str) -> Result<u64> {
        self.log.append_event_async(&SessionLogEntry::InteractionRequested {
            tool_round_seq: self.round, tool_call_id: "call-1".into(), interaction_id: id.into(),
            producer: "middleware:test".into(), step_path: vec![], key: id.into(), kind: kind.into(),
            payload: json!({"message": format!("Public message for {id}?"), "private": "secret checkpoint input"}),
            summary: "Summary fallback".into(), chain_version: "v1".into(), checkpoint: "private-checkpoint".into(),
            fence_token: self.lease.fence_token(),
        }).await
    }

    pub async fn ready(&self, ids: &[&str]) -> Result<()> {
        self.log
            .append_event_async(&SessionLogEntry::SuspensionReady {
                tool_round_seq: self.round,
                pending: ids.iter().map(|id| id.to_string()).collect(),
                fence_token: self.lease.fence_token(),
            })
            .await?;
        Ok(())
    }

    pub async fn permission(&mut self) -> Result<RequestPermissionRequest> {
        tokio::time::timeout(TEST_TIMEOUT, self.client.permissions.recv())
            .await?
            .context("permission stream closed")
    }

    pub fn reply(&self, id: &str) -> Result<()> {
        self.client.permission_responses.send(selected(id))?;
        Ok(())
    }

    pub async fn answer(
        &self,
        interaction_id: &str,
        option_id: &str,
    ) -> Result<InteractionResponse> {
        self.reply(option_id)?;
        self.decision(interaction_id).await
    }

    pub async fn decision(&self, id: &str) -> Result<InteractionResponse> {
        tokio::time::timeout(TEST_TIMEOUT, async {
            loop {
                for (_, entry) in self.log.load_events_latest_async().await? {
                    if let SessionLogEntry::InteractionResponse {
                        interaction_id,
                        response,
                        ..
                    } = entry
                    {
                        if interaction_id == id {
                            return Ok(response);
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?
    }

    pub async fn reported(&mut self, outcome: &str) -> Result<()> {
        tokio::time::timeout(TEST_TIMEOUT, async {
            loop {
                let notice = self
                    .client
                    .notifications
                    .recv()
                    .await
                    .context("notice stream closed")?;
                if let Some(text) = notification_text(notice) {
                    if text.contains("response rejected") && text.contains(outcome) {
                        return Ok(());
                    }
                }
            }
        })
        .await?
    }

    pub async fn quiet(&mut self) {
        assert!(
            tokio::time::timeout(QUIET, self.client.permissions.recv())
                .await
                .is_err(),
            "no other modal may be shown"
        );
    }
}

pub(super) fn selected(id: &str) -> RequestPermissionOutcome {
    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(PermissionOptionId::new(
        id.to_string(),
    )))
}
