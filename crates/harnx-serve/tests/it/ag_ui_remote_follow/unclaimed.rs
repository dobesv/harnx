use super::*;
use crate::common;
use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent};
use harnx_runtime::nats_event_sink::{events_subject, AdvisoryEnvelope};

const AGENT: &str = "plain@unclaimed";

struct Unclaimed {
    _sandbox: TestConfigSandbox,
    _nats: common::NatsServerHandle,
    config: Config,
    session: String,
    key: String,
    log: NatsSessionLog,
}

impl Unclaimed {
    async fn new(timeout_secs: u64) -> Option<Self> {
        let sandbox = TestConfigSandbox::new();
        let nats = common::spawn_nats_server().await.unwrap()?;
        sandbox.write_nats_server("unclaimed", &format!("url: {:?}\n", nats.url()));
        let mut config = sandbox.config();
        config.data.nats_lease_acquisition_timeout_secs = timeout_secs;
        let session = format!("unclaimed-{}", Uuid::new_v4());
        let key = harnx_core::session_identity::session_key(Some("plain"), &session);
        let log = NatsSessionLog::new(config.nats_jetstream("unclaimed").await.unwrap(), &key);
        let fixture = Self {
            _sandbox: sandbox,
            _nats: nats,
            config,
            session,
            key,
            log,
        };
        let admission = fixture.rpc(json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"text":"unclaimed input"}})).await;
        assert_eq!(admission["result"]["status"], "accepted", "{admission}");
        Some(fixture)
    }

    async fn rpc(&self, request: serde_json::Value) -> serde_json::Value {
        let registry = SessionRegistry::new(self.config.clone());
        let response = handle_ag_ui_rpc_bytes(
            http::Method::POST,
            AGENT,
            &self.session,
            bytes::Bytes::from(request.to_string()),
            &self.config,
            &registry,
            PersistenceKind::Nats,
        )
        .await
        .unwrap();
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
    }

    async fn attach(&self) -> AppResponse {
        let registry = SessionRegistry::new(self.config.clone());
        ag_ui_run_with_call_fn(
            &self.config,
            &registry,
            AGENT,
            &self.session,
            &serde_json::to_vec(
                &json!({"threadId":Uuid::new_v4(),"runId":Uuid::new_v4(),"messages":[]}),
            )
            .unwrap(),
            None,
        )
        .await
        .unwrap()
    }

    async fn claim(&self) -> NatsSessionLease {
        NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: self.log.jetstream().clone(),
            session_id: &self.key,
            worker_id: "delayed-worker".into(),
            generation: 1,
            config: NatsLeaseConfig::default(),
            session_metadata: None,
        })
        .await
        .unwrap()
        .expect("claim")
    }

    async fn finish(&self) {
        let seq = harnx_core::session_reconstruct::latest_prompt_seq(
            &self.log.load_events_latest_async().await.unwrap(),
        )
        .unwrap();
        self.log
            .append_event_async(&SessionLogEntry::Message {
                id: Some("reply".into()),
                role: MessageRole::Assistant,
                content: MessageContent::Text("durable reply".into()),
                timestamp: None,
                fence_token: Some(1),
            })
            .await
            .unwrap();
        self.log
            .append_event_async(&SessionLogEntry::TurnEnd {
                through_seq: seq,
                fence_token: 1,
                timestamp: None,
                usage: None,
            })
            .await
            .unwrap();
    }

    async fn publish_chunk(&self) {
        let client = self.config.nats_client("unclaimed").await.unwrap();
        let after_seq = self
            .log
            .load_events_latest_async()
            .await
            .unwrap()
            .last()
            .unwrap()
            .0;
        let envelope = AdvisoryEnvelope::new(
            after_seq,
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("live delayed reply".into())],
            }),
        );
        client
            .publish(
                events_subject(&self.key),
                envelope.to_bytes().unwrap().into(),
            )
            .await
            .unwrap();
        client.flush().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unclaimed_prompt_reports_running_and_attach_waits_for_claim() {
    let Some(fixture) = Unclaimed::new(15).await else {
        return;
    };
    let get = fixture
        .rpc(json!({"jsonrpc":"2.0","id":2,"method":"session/get"}))
        .await;
    assert_eq!(get["result"]["state"], json!({"status":"running"}));
    let response = fixture.attach().await;
    let reader = tokio::spawn(read_sse_until(response, Duration::from_secs(10), |read| {
        has_event(&read.events, "RUN_FINISHED")
    }));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!reader.is_finished(), "attach must not finish before claim");
    let lease = fixture.claim().await;
    fixture.publish_chunk().await;
    // Wait across a lease poll before completing, so this tests the claim transition.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    fixture.finish().await;
    lease.release().await.unwrap();
    let read = reader.await.unwrap();
    assert!(has_event(&read.events, "RUN_FINISHED"), "{:?}", read.events);
    assert!(!has_event(&read.events, "RUN_ERROR"));
    assert!(read
        .events
        .iter()
        .any(|event| event["delta"] == "live delayed reply"));
    let get = fixture
        .rpc(json!({"jsonrpc":"2.0","id":3,"method":"session/get"}))
        .await;
    assert_eq!(get["result"]["state"]["status"], "idle");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unclaimed_prompt_completed_between_polls_hydrates_before_finishing() {
    let Some(fixture) = Unclaimed::new(15).await else {
        return;
    };
    let response = fixture.attach().await;
    let reader = tokio::spawn(read_sse_until(response, Duration::from_secs(5), |read| {
        has_event(&read.events, "RUN_FINISHED")
    }));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!reader.is_finished());
    fixture.finish().await;
    let read = reader.await.unwrap();
    assert!(has_event(&read.events, "RUN_FINISHED"), "{:?}", read.events);
    assert!(
        read.events
            .iter()
            .filter(|event| event["type"] == "MESSAGES_SNAPSHOT")
            .any(|event| event["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|msg| msg["content"] == "durable reply")),
        "{:?}",
        read.events
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unclaimed_prompt_has_bounded_attach_without_false_run_finished() {
    let Some(fixture) = Unclaimed::new(1).await else {
        return;
    };
    let response = fixture.attach().await;
    let read = read_sse_until(response, Duration::from_secs(5), |read| {
        has_event(&read.events, "RUN_ERROR")
    })
    .await;
    assert!(has_event(&read.events, "RUN_ERROR"), "{:?}", read.events);
    assert!(!has_event(&read.events, "RUN_FINISHED"));
    assert!(read.events.iter().any(|event| event["message"]
        .as_str()
        .is_some_and(|message| message.contains("No worker claimed"))));
    // Observation timeout must not alter worker-owned durable work.
    assert!(harnx_core::session_reconstruct::pending_prompt_seq(
        &fixture.log.load_events_latest_async().await.unwrap()
    )
    .is_some());
    let get = fixture
        .rpc(json!({"jsonrpc":"2.0","id":2,"method":"session/get"}))
        .await;
    assert_eq!(get["result"]["state"]["status"], "running");
    let cancel = fixture
        .rpc(json!({"jsonrpc":"2.0","id":3,"method":"session/cancel"}))
        .await;
    assert_eq!(cancel["result"]["outcome"], "accepted", "{cancel}");
    let settled = fixture
        .rpc(json!({"jsonrpc":"2.0","id":4,"method":"session/get"}))
        .await;
    assert_eq!(settled["result"]["state"]["status"], "interrupted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unclaimed_prompt_cancel_settles_attach_and_rpc_state() {
    let Some(fixture) = Unclaimed::new(15).await else {
        return;
    };
    let response = fixture.attach().await;
    fixture
        .log
        .append_event_async(&SessionLogEntry::cancel_request(
            "stop".into(),
            "test".into(),
        ))
        .await
        .unwrap();
    let read = read_sse_until(response, Duration::from_secs(5), |read| {
        has_event(&read.events, "RUN_FINISHED")
    })
    .await;
    assert!(has_event(&read.events, "RUN_FINISHED"));
    let get = fixture
        .rpc(json!({"jsonrpc":"2.0","id":3,"method":"session/get"}))
        .await;
    assert_eq!(get["result"]["state"]["status"], "interrupted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unclaimed_prompt_durable_error_ends_attach_without_claim() {
    let Some(fixture) = Unclaimed::new(15).await else {
        return;
    };
    let response = fixture.attach().await;
    fixture
        .log
        .append_event_async(&SessionLogEntry::Error {
            message: "activation failed".into(),
            fence_token: 1,
            timestamp: None,
        })
        .await
        .unwrap();
    let read = read_sse_until(response, Duration::from_secs(5), |read| {
        has_event(&read.events, "RUN_ERROR")
    })
    .await;
    assert!(!has_event(&read.events, "RUN_FINISHED"));
    assert!(read
        .events
        .iter()
        .any(|event| event["type"] == "RUN_ERROR" && event["message"] == "activation failed"));
    let get = fixture
        .rpc(json!({"jsonrpc":"2.0","id":3,"method":"session/get"}))
        .await;
    assert_eq!(get["result"]["state"]["status"], "idle");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unclaimed_prompt_retraction_settles_attach_without_claim() {
    let Some(fixture) = Unclaimed::new(15).await else {
        return;
    };
    let response = fixture.attach().await;
    let seq = harnx_core::session_reconstruct::pending_prompt_seq(
        &fixture.log.load_events_latest_async().await.unwrap(),
    )
    .unwrap() as usize;
    fixture
        .log
        .append_event_async(&SessionLogEntry::EditEntries {
            from: seq,
            to: seq,
            replacements: vec![],
        })
        .await
        .unwrap();
    let read = read_sse_until(response, Duration::from_secs(5), |read| {
        has_event(&read.events, "RUN_FINISHED")
    })
    .await;
    assert!(has_event(&read.events, "RUN_FINISHED"), "{:?}", read.events);
    assert!(!has_event(&read.events, "RUN_ERROR"));
    let get = fixture
        .rpc(json!({"jsonrpc":"2.0","id":3,"method":"session/get"}))
        .await;
    assert_eq!(get["result"]["state"]["status"], "idle");
}
