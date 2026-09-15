use super::*;
use crate::test_support::{seed_nats_session, NatsSessionSeed, TestConfigSandbox};
use harnx_core::{
    event::{AgentEvent, ContentBlock, ModelEvent},
    message::{Message, MessageContent, MessageRole},
};
use harnx_execution_control::{ExecutionStore, InterruptScope, Owner};

struct Fixture {
    _sandbox: TestConfigSandbox,
    config: Config,
    store: ExecutionStore,
    jetstream: JetstreamContext,
    stream: SessionEventStream,
}

impl Fixture {
    async fn new() -> Self {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();
        assert!(
            seed_nats_session(
                &config,
                NatsSessionSeed {
                    agent: "plain",
                    session_id: "blocked-sse",
                    messages: &[Message {
                        id: Some("input".into()),
                        role: MessageRole::User,
                        content: MessageContent::Text("blocked stream".into()),
                        ..Default::default()
                    }],
                }
            )
            .await,
            "NATS required"
        );
        let jetstream = config.nats_jetstream(LOCAL_CLUSTER_KEY).await.unwrap();
        let store = ExecutionStore::ensure(&jetstream, 1).await.unwrap();
        let original = store
            .session(&storage_key(), None, Some("g1"))
            .await
            .unwrap();
        store
            .reserve_prompt(&original.reference, "input")
            .await
            .unwrap();
        store
            .commit_prompt(&original.reference, "input", 1)
            .await
            .unwrap();
        store
            .claim(
                &original.reference,
                Owner {
                    instance_id: "worker".into(),
                    fence: 1,
                },
            )
            .await
            .unwrap();
        store.activate_gate(&original.reference).await.unwrap();
        let client = config.nats_client(LOCAL_CLUSTER_KEY).await.unwrap();
        let stream = SessionEventStream::attach(jetstream.clone(), client, &storage_key())
            .await
            .unwrap();
        Self {
            _sandbox: sandbox,
            config,
            store,
            jetstream,
            stream,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepted_stop_interrupts_a_backpressured_remote_output_queue() {
    let mut fixture = Fixture::new().await;
    let generation =
        RemoteGeneration::bind(&mut fixture.stream, &fixture.jetstream, &storage_key())
            .await
            .unwrap();
    let live = fixture.stream.live_state().clone();
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let probe = tx.clone();
    let task = tokio::spawn(remote_follow_task(FollowTaskParams {
        generation,
        event_stream: fixture.stream,
        jetstream: fixture.jetstream,
        session_id: storage_key(),
        tx,
        through_seq: 1,
    }));
    let client = fixture.config.nats_client(LOCAL_CLUSTER_KEY).await.unwrap();
    let envelope = AdvisoryEnvelope::new(
        1,
        AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("queued delta".into())],
        }),
    )
    .with_execution_id("g1");
    client
        .publish(
            harnx_runtime::nats_event_sink::events_subject(&storage_key()),
            envelope.to_bytes().unwrap().into(),
        )
        .await
        .unwrap();
    // Capacity is the phase seam: START is queued and CONTENT cannot fit. Do not
    // drain it to make cancellation progress. Timeouts only bound test failure.
    tokio::time::timeout(Duration::from_secs(3), async {
        while probe.capacity() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let reference = harnx_execution_control::OperationRef::new(storage_key(), "g1");
    let context = fixture.store.activate_gate(&reference).await.unwrap();
    fixture
        .store
        .interrupt(
            &InterruptScope {
                gate_root: context.gate_root().clone(),
                operation: reference,
                reason: "queue stop".into(),
            },
            "stop",
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(live.is_stopped("g1"));
    drop(probe);
    let frames: Vec<_> = event_frames(rx, live).collect().await;
    assert!(
        frames.is_empty(),
        "unseen lifecycle start cannot leave an orphan end"
    );
}

fn storage_key() -> String {
    harnx_core::session_identity::session_key(Some("plain"), "blocked-sse")
}
