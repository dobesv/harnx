use super::*;
use crate::ag_ui_rpc::{handle_ag_ui_rpc_bytes, PersistenceKind};
use crate::{ag_ui_interactions, test_support::TestConfigSandbox};
use bytes::Bytes;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::{
    config::Config,
    interactions::accept_interaction_response,
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session_log::NatsSessionLog,
    nats_session_metadata::{SessionMetadata, SessionMetadataStore},
    nats_worker::{control_subject, ControlCommand, NatsSessionLogBackend},
    SessionInitializer,
};
use http::Method;
use std::{sync::Arc, time::Duration};
use tokio_stream::StreamExt;
use tokio_util::task::AbortOnDropHandle;

async fn remote_config(sandbox: &TestConfigSandbox) -> Config {
    sandbox.write_agent("plain", "You are plain.");
    assert!(
        crate::test_support::ensure_test_nats().await,
        "NATS required"
    );
    let server = harnx_runtime::config::resolve_local_nats_server_config()
        .await
        .unwrap();
    sandbox.write_nats_server("shared", &format!(
        "url: {}\ntoken: {}\nagents:\n  - name: plain\n    description: Interaction test agent\n    role: assistant\n",
        serde_json::to_string(&server.url).unwrap(), serde_json::to_string(&server.token).unwrap(),
    ));
    sandbox.config()
}

struct Fixture {
    config: Config,
    registry: SessionRegistry,
    log: NatsSessionLog,
    lease: Arc<NatsSessionLease>,
    _control: AbortOnDropHandle<()>,
}

impl Fixture {
    async fn new(sandbox: &TestConfigSandbox) -> Self {
        let config = remote_config(sandbox).await;
        let client = config.nats_client("shared").await.unwrap();
        let js = config.nats_jetstream("shared").await.unwrap();
        let mut scoped = config.clone();
        scoped.use_agent_by_name("plain").unwrap();
        let metadata = SessionMetadataStore::ensure(&js, 1).await.unwrap();
        metadata
            .create(&SessionMetadata::new(
                "interaction-rpc",
                SessionInitializer::from_config(&scoped).unwrap(),
            ))
            .await
            .unwrap();
        let key = harnx_core::session_identity::session_key(Some("plain"), "interaction-rpc");
        let log = NatsSessionLog::new(js.clone(), key.clone()).with_replicas(1);
        for (expected, entry) in ag_ui_interactions::tests::entries() {
            assert_eq!(log.append_event_async(&entry).await.unwrap(), expected);
        }
        let lease = Arc::new(
            NatsSessionLease::acquire(NatsLeaseAcquireParams {
                jetstream: js.clone(),
                session_id: &key,
                worker_id: "serve-interaction-test".into(),
                generation: 1,
                config: NatsLeaseConfig::default(),
                session_metadata: Some(metadata),
            })
            .await
            .unwrap()
            .expect("lease"),
        );
        let backend = NatsSessionLogBackend::new(js, key.clone(), 1);
        let mut commands = client.subscribe(control_subject(&key)).await.unwrap();
        client.flush().await.unwrap();
        let held = lease.clone();
        let control = tokio::spawn(async move {
            while let Some(message) = commands.next().await {
                let ControlCommand::InteractionResponse {
                    interaction_id,
                    request_seq,
                    response,
                } = ControlCommand::from_bytes(&message.payload).unwrap()
                else {
                    panic!("unexpected command");
                };
                let result = accept_interaction_response(
                    &backend,
                    &held,
                    &interaction_id,
                    request_seq,
                    &response,
                )
                .await
                .unwrap();
                client
                    .publish(
                        message.reply.unwrap(),
                        serde_json::to_vec(&result).unwrap().into(),
                    )
                    .await
                    .unwrap();
            }
        });
        let registry = SessionRegistry::new(config.clone());
        Self {
            config,
            registry,
            log,
            lease,
            _control: AbortOnDropHandle::new(control),
        }
    }

    async fn respond(&self, id: &str, seq: u64, action: &str) -> Value {
        self.respond_with_note(id, seq, action, "operator note")
            .await
    }

    async fn respond_with_note(&self, id: &str, seq: u64, action: &str, note: &str) -> Value {
        let request = json!({ "jsonrpc":"2.0", "id":1, "method":"session/interaction_response",
            "params": { "interaction_id":id, "request_seq":seq, "response": {"action":action,"data":{},"note":note} } });
        let response = tokio::time::timeout(
            Duration::from_secs(30),
            handle_ag_ui_rpc_bytes(
                Method::POST,
                "plain@shared",
                "interaction-rpc",
                Bytes::from(request.to_string()),
                &self.config,
                &self.registry,
                PersistenceKind::Nats,
            ),
        )
        .await
        .expect("bounded RPC")
        .unwrap();
        body(response).await
    }

    async fn pending(&self) -> Option<Value> {
        ag_ui_interactions::pending_value(&self.log.load_events_latest_async().await.unwrap())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn interaction_rpc_durable_acceptance_approve_deny_idempotency_and_stale_sequence() {
    let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    let fixture = Fixture::new(&sandbox).await;
    assert!(fixture.pending().await.is_none());
    let unready = fixture.respond("first", 3, "approve").await;
    assert_eq!(unready["error"]["data"]["outcome"], "stale");
    fixture
        .log
        .append_event_async(&ag_ui_interactions::tests::ready())
        .await
        .unwrap();
    assert_eq!(fixture.pending().await.unwrap()["interaction_id"], "first");
    let wrong = fixture.respond("first", 4, "approve").await;
    assert_eq!(wrong["error"]["data"]["outcome"], "wrong_sequence");
    let applied = fixture.respond("first", 3, "approve").await;
    assert_eq!(
        applied["result"],
        json!({"applied":true,"outcome":"applied"})
    );
    let retry = fixture.respond("first", 3, "approve").await;
    assert_eq!(
        retry["result"],
        json!({"applied":true,"outcome":"already_applied"})
    );
    assert_recorded_conflicts(&fixture).await;
    assert_eq!(fixture.pending().await.unwrap()["interaction_id"], "second");
    let denied = fixture.respond("second", 4, "deny").await;
    assert_eq!(denied["result"]["outcome"], "applied");
    assert!(fixture.pending().await.is_none());
    let entries = fixture.log.load_events_latest_async().await.unwrap();
    let responses: Vec<_> = entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::InteractionResponse { response, .. } => Some(response),
            _ => None,
        })
        .collect();
    assert_eq!(responses.len(), 2, "retry must not append a second answer");
    assert_eq!(responses[0].action, "approve");
    assert_eq!(responses[1].action, "deny");
    assert_eq!(responses[1].note.as_deref(), Some("operator note"));
    fixture.lease.release().await.unwrap();
}

async fn attach(fixture: &Fixture) -> AppResponse {
    crate::ag_ui::ag_ui_run_with_call_fn(
        &fixture.config,
        &fixture.registry,
        "plain@shared",
        "interaction-rpc",
        br#"{"messages":[]}"#,
        None,
    )
    .await
    .expect("AG-UI attach")
}

async fn stream_events(response: AppResponse) -> Vec<Value> {
    let bytes = tokio::time::timeout(Duration::from_secs(30), response.into_body().collect())
        .await
        .expect("modal boundary must finish attachment")
        .unwrap()
        .to_bytes();
    String::from_utf8(bytes.to_vec())
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn interaction_ag_ui_live_stream_and_reconnect_expose_one_ready_request_at_a_time() {
    let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    let fixture = Fixture::new(&sandbox).await;
    // Attach before the manifest. The stream must pick up the durable boundary,
    // even though no advisory notification accompanies this append.
    let response = attach(&fixture).await;
    fixture
        .log
        .append_event_async(&ag_ui_interactions::tests::ready())
        .await
        .unwrap();
    let events = stream_events(response).await;
    let pending: Vec<_> = events
        .iter()
        .filter(|event| event["name"] == "pending_interaction")
        .collect();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["value"]["interaction_id"], "first");
    assert_eq!(pending[0]["value"]["request_seq"], 3);
    assert_eq!(pending[0]["value"]["kind"], "confirm");
    assert_eq!(pending[0]["value"]["message"], "Run this tool?");
    assert_eq!(pending[0]["value"]["pending_count"], 1);
    assert_eq!(events.last().unwrap()["type"], "RUN_FINISHED");
    assert!(events.iter().all(|event| event["type"] != "RUN_ERROR"));
    assert_eq!(
        fixture.respond("first", 3, "approve").await["result"]["outcome"],
        "applied"
    );
    let reconnect = stream_events(attach(&fixture).await).await;
    let pending: Vec<_> = reconnect
        .iter()
        .filter(|event| event["name"] == "pending_interaction")
        .collect();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["value"]["interaction_id"], "second");
    assert_eq!(pending[0]["value"]["request_seq"], 4);
    assert_eq!(pending[0]["value"]["pending_count"], 0);
    assert_eq!(pending[0]["value"]["kind"], "form");
    fixture.lease.release().await.unwrap();
}

async fn assert_recorded_conflicts(fixture: &Fixture) {
    for (action, note) in [("deny", "operator note"), ("approve", "different note")] {
        let conflict = fixture.respond_with_note("first", 3, action, note).await;
        assert_eq!(conflict["error"]["data"]["outcome"], "conflicting");
        assert_eq!(
            conflict["error"]["data"]["response"],
            json!({
                "action":"approve","data":{},"note":"operator note",
            })
        );
    }
}
