use super::*;
use crate::test_support::{seed_nats_session, NatsSessionSeed, TestConfigSandbox};
use harnx_core::{
    event::{AgentEvent, ContentBlock, ModelEvent},
    message::{Message, MessageContent, MessageRole},
    session::SessionLogEntry,
};
use harnx_runtime::config::LOCAL_CLUSTER_KEY;
use harnx_runtime::nats_session_log::NatsSessionLog;

struct Fixture {
    _sandbox: TestConfigSandbox,
    config: Config,
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
        let client = config.nats_client(LOCAL_CLUSTER_KEY).await.unwrap();
        let stream = SessionEventStream::attach(jetstream.clone(), client, &storage_key())
            .await
            .unwrap();
        Self {
            _sandbox: sandbox,
            config,
            jetstream,
            stream,
        }
    }
}

/// Interrupt the way a frontend does: one `Cancel` appended to the log.
async fn interrupt(jetstream: &JetstreamContext) {
    NatsSessionLog::new(jetstream.clone(), storage_key())
        .append_event_async(&SessionLogEntry::cancel_request(
            "queue-stop".into(),
            "client:test".into(),
        ))
        .await
        .unwrap();
}

/// Two claims about a reader that was already attached when the interrupt
/// landed. The stop watch has to stay pollable while a full output queue holds
/// the follower — nothing will ever drain that queue if the interrupt cannot
/// get through it — and the chunk waiting in that queue belongs to the turn the
/// `Cancel` stopped. Its sequence clears the reader's durable position, so the
/// fence is the only thing that can hold it back: the assertion reads through a
/// fork, which keeps the fence and drops this attachment's detachment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepted_stop_interrupts_a_backpressured_remote_output_queue() {
    let fixture = Fixture::new().await;
    // The seeded prompt is the only user message: sequence 1 is this follow's.
    let interrupt_watch = RemoteInterruptWatch::bind(&fixture.jetstream, &storage_key(), 1);
    let live = fixture.stream.live_state().clone();
    let attached_seq = fixture.stream.last_applied_seq();
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let probe = tx.clone();
    let task = tokio::spawn(remote_follow_task(FollowTaskParams {
        interrupt_watch,
        event_stream: fixture.stream,
        jetstream: fixture.jetstream.clone(),
        session_id: storage_key(),
        tx,
        through_seq: 1,
    }));
    let client = fixture.config.nats_client(LOCAL_CLUSTER_KEY).await.unwrap();
    let envelope = AdvisoryEnvelope::new(
        attached_seq,
        AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("queued delta".into())],
        }),
    );
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
    interrupt(&fixture.jetstream).await;
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(probe);
    let frames: Vec<_> = event_frames(rx, live.clone(), attached_seq).collect().await;
    assert!(
        frames.is_empty(),
        "unseen lifecycle start cannot leave an orphan end"
    );
    assert!(
        !live.fork().should_render(&envelope, attached_seq),
        "the accepted Cancel fences the stopped turn's output for later readers too"
    );
}

fn storage_key() -> String {
    harnx_core::session_identity::session_key(Some("plain"), "blocked-sse")
}
