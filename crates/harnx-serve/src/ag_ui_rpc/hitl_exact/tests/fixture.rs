use super::*;
use crate::ag_ui_rpc::{handle_ag_ui_rpc_bytes, PersistenceKind};
use crate::test_support::TestConfigSandbox;
use bytes::Bytes;
use harnx_core::{abort::create_abort_signal, session::SessionLogEntry, tool::ToolCall};
use harnx_runtime::{
    config::Config,
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session_log::NatsSessionLog,
    nats_worker::{control_subject, ControlCommand, FencedSessionLogSink, NatsSessionLogBackend},
    NatsSession, NatsSessionConfig, SessionActivationRoute, SessionInitializer,
};
use http::{Method, StatusCode};
use http_body_util::BodyExt;
use std::{sync::Arc, time::Duration};
use tokio_stream::StreamExt;
use tokio_util::task::AbortOnDropHandle;

pub(super) const CALL: &str = "reused-provider-call";
pub(super) const SESSION: &str = "exact-child-rpc";

pub(super) struct Fixture {
    pub config: Config,
    pub registry: SessionRegistry,
    pub log: NatsSessionLog,
    pub lease: Arc<NatsSessionLease>,
    pub reference: ApprovalRef,
    _control: AbortOnDropHandle<()>,
}

async fn config(sandbox: &TestConfigSandbox) -> Config {
    sandbox.write_agent("plain", "You are plain.");
    assert!(
        crate::test_support::ensure_test_nats().await,
        "NATS required"
    );
    let server = harnx_runtime::config::resolve_local_nats_server_config()
        .await
        .unwrap();
    sandbox.write_nats_server("shared", &format!(
        "url: {}\ntoken: {}\nagents:\n  - name: plain\n    description: Exact decision test\n    role: assistant\n",
        serde_json::to_string(&server.url).unwrap(), serde_json::to_string(&server.token).unwrap(),
    ));
    sandbox.config()
}

impl Fixture {
    pub async fn new(sandbox: &TestConfigSandbox) -> Self {
        let config = config(sandbox).await;
        let client = config.nats_client("shared").await.unwrap();
        let js = config.nats_jetstream("shared").await.unwrap();
        let mut scoped = config.clone();
        scoped.use_agent_by_name("plain").unwrap();
        let source = NatsSession::new(
            NatsSessionConfig {
                cluster: "shared".into(),
                initializer: SessionInitializer::from_config(&scoped).unwrap(),
                session_id: Some(SESSION.into()),
                activation_route: SessionActivationRoute::ClusterShared,
            },
            client.clone(),
            js.clone(),
            create_abort_signal(),
        )
        .await
        .unwrap();
        let prompt = source
            .clone()
            .with_external_admission()
            .enqueue_text("approve child tool")
            .await
            .unwrap();
        let store = harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&js, 1)
            .await
            .unwrap();
        let log = NatsSessionLog::new(js.clone(), source.storage_key()).with_replicas(1);
        let entries = log.load_events_latest_async().await.unwrap();
        let prompt_id = entries
            .iter()
            .find_map(|(seq, entry)| match entry {
                SessionLogEntry::Message { id: Some(id), .. } if *seq == prompt.user_msg_seq() => {
                    Some(id)
                }
                _ => None,
            })
            .expect("admitted prompt");
        let admission = store
            .prompt_admission(source.storage_key(), prompt_id)
            .await
            .unwrap()
            .unwrap();
        let lease = Arc::new(
            NatsSessionLease::acquire(NatsLeaseAcquireParams {
                jetstream: js.clone(),
                session_id: source.storage_key(),
                worker_id: "exact-rpc-worker".into(),
                generation: 1,
                config: NatsLeaseConfig::default(),
                session_metadata: None,
            })
            .await
            .unwrap()
            .expect("lease"),
        );
        let (round, request) = append_pending(&log, lease.fence_token()).await;
        let reference = ApprovalRef {
            invocation_id: admission.invocation_id.as_str().to_owned(),
            tool_round_seq: round,
            request_seq: request,
            tool_call_id: CALL.into(),
        };
        let backend = NatsSessionLogBackend::new(js, source.storage_key(), 1);
        let sink = FencedSessionLogSink::new(backend.clone(), lease.clone());
        let mut commands = client
            .subscribe(control_subject(source.storage_key()))
            .await
            .unwrap();
        client.flush().await.unwrap();
        let fence_token = lease.fence_token();
        let invocation_id = reference.invocation_id.clone();
        let control = tokio::spawn(async move {
            while let Some(message) = commands.next().await {
                let ControlCommand::HitlApprovalDecision {
                    tool_call_id,
                    approved,
                    note,
                    request_seq,
                } = ControlCommand::from_bytes(&message.payload).unwrap()
                else {
                    panic!("exact legacy command")
                };
                let entries = backend.load_events_latest_async().await.unwrap();
                let current = pending_legacy(&entries, &invocation_id)
                    .expect("decision must target pending request");
                assert_eq!(tool_call_id, current.tool_call_id);
                assert_eq!(request_seq, Some(current.request_seq));
                let tail = entries.last().unwrap().0;
                assert!(sink
                    .append_hitl_event_cas(
                        &SessionLogEntry::HitlApprovalDecision {
                            tool_call_id,
                            approved,
                            note,
                            fence_token,
                        },
                        tail
                    )
                    .await
                    .unwrap()
                    .is_some());
                client
                    .publish(message.reply.unwrap(), Bytes::new())
                    .await
                    .unwrap();
            }
        });
        Self {
            registry: SessionRegistry::new(config.clone()),
            config,
            log,
            lease,
            reference,
            _control: AbortOnDropHandle::new(control),
        }
    }

    pub async fn rpc(&self, method: &str, params: Value) -> (StatusCode, Value) {
        let response = tokio::time::timeout(
            Duration::from_secs(30),
            handle_ag_ui_rpc_bytes(
                Method::POST,
                "plain@shared",
                SESSION,
                Bytes::from(
                    json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string(),
                ),
                &self.config,
                &self.registry,
                PersistenceKind::Nats,
            ),
        )
        .await
        .expect("bounded RPC")
        .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap())
    }

    pub async fn answer(&self, request: &ApprovalRef, approved: bool, note: &str) -> Value {
        let (status, body) = self
            .rpc(
                "session/hitl_decision_exact",
                json!({
                    "request":request,"approved":approved,"note":note,
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["result"].clone()
    }

    pub async fn advance(&self) -> ApprovalRef {
        self.log
            .append_event_async(&SessionLogEntry::ToolResults {
                results: vec![],
                handoff_selection: None,
                timestamp: None,
            })
            .await
            .unwrap();
        let (round, request) = append_pending(&self.log, self.lease.fence_token()).await;
        ApprovalRef {
            tool_round_seq: round,
            request_seq: request,
            ..self.reference.clone()
        }
    }

    pub async fn entries(&self) -> Vec<(u64, SessionLogEntry)> {
        self.log.load_events_latest_async().await.unwrap()
    }
}

async fn append_pending(log: &NatsSessionLog, fence_token: u64) -> (u64, u64) {
    let round = log
        .append_event_async(&SessionLogEntry::ToolCalls {
            text: "approval needed".into(),
            thought: None,
            calls: vec![ToolCall::new(
                "read".into(),
                json!({}),
                Some(CALL.into()),
                None,
            )],
            timestamp: None,
            fence_token: Some(fence_token),
        })
        .await
        .unwrap();
    let request = log
        .append_event_async(&SessionLogEntry::HitlApprovalRequested {
            tool_call_id: CALL.into(),
            summary: "Read the child file?".into(),
            fence_token,
        })
        .await
        .unwrap();
    (round, request)
}

pub(super) fn pending_legacy(
    entries: &[(u64, SessionLogEntry)],
    invocation_id: &str,
) -> Option<ApprovalRef> {
    let summary = harnx_runtime::interactions::derive_pending_interaction_summary(
        entries,
        "plain",
        "shared",
        SESSION,
        invocation_id,
    )
    .unwrap()?;
    let harnx_core::pending_interaction::PendingInteractionRequest::LegacyApproval { request } =
        summary.request
    else {
        panic!("legacy fixture")
    };
    Some(request)
}
