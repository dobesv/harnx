//! A remote follow ends on the session log's `Cancel`, not on a worker
//! acknowledging it.
//!
//! The reader has no worker of its own and no lease: the durable `Cancel` is
//! the only thing that can finish its run, whether it was already attached
//! when the interrupt landed or attaches afterwards.
use super::*;
use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent};
use harnx_runtime::nats_event_sink::{events_subject, AdvisoryEnvelope};
use harnx_runtime::nats_session_log::NatsSessionLog as Log;

struct InterruptedRemote {
    session: LeasedSession,
}

impl InterruptedRemote {
    async fn new() -> Self {
        let session = seed_in_progress_leased_session()
            .await
            .expect("NATS required");
        Self { session }
    }

    fn log(&self) -> Log {
        Log::new(self.session.jetstream.clone(), storage_key(&self.session))
    }

    /// Interrupt the way a frontend does: one `Cancel` appended to the log.
    async fn interrupt(&self) {
        self.log()
            .append_event_async(&SessionLogEntry::cancel_request(
                "remote-cancel".into(),
                "client:test".into(),
            ))
            .await
            .unwrap();
    }

    /// The next prompt, typed after the interruption. It starts a turn of its
    /// own and must not revive the interrupted one for this reader.
    async fn next_prompt(&self) {
        self.log()
            .append_event_async(&SessionLogEntry::Message {
                id: Some("next-input".into()),
                role: MessageRole::User,
                content: MessageContent::Text("replacement prompt".into()),
                timestamp: None,
                fence_token: None,
            })
            .await
            .unwrap();
    }

    async fn entries(&self) -> Vec<(u64, SessionLogEntry)> {
        self.log().load_events_latest_async().await.unwrap()
    }

    /// Publish a live chunk stamped with the session-log position the worker
    /// had reached when it produced it.
    async fn publish_chunk(&self, after_seq: u64, text: &str) {
        let client = self
            .session
            .config
            .nats_client(LOCAL_CLUSTER_KEY)
            .await
            .unwrap();
        let envelope = AdvisoryEnvelope::new(
            after_seq,
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text(text.into())],
            }),
        );
        client
            .publish(
                events_subject(&storage_key(&self.session)),
                envelope.to_bytes().unwrap().into(),
            )
            .await
            .unwrap();
        client.flush().await.unwrap();
    }
}

/// The interrupted turn never writes a `TurnEnd`, and the worker that was
/// running it keeps its lease. The reader still finishes: the log's `Cancel` is
/// the only thing it needs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupted_remote_run_finishes_without_a_turn_end_while_the_lease_is_held() {
    let fixture = InterruptedRemote::new().await;
    let response = open_promptless_sse(&fixture.session).await;
    fixture.interrupt().await;
    fixture.next_prompt().await;

    let read = read_sse_until(response, Duration::from_secs(5), |read| {
        has_event(&read.events, "RUN_FINISHED")
    })
    .await;
    assert!(
        has_event(&read.events, "RUN_FINISHED"),
        "the log's Cancel must end a remote follow: {:?}",
        read.events
    );
    assert!(
        !fixture
            .entries()
            .await
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::TurnEnd { .. })),
        "an interrupted turn never reaches a TurnEnd"
    );
    assert!(fixture.session.lease.revalidate_ownership().await.unwrap());
    fixture.session.lease.release().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_attach_after_acceptance_does_not_wait_for_original_lease_cleanup() {
    let fixture = InterruptedRemote::new().await;
    fixture.interrupt().await;
    // The original lease is deliberately still held: a reader attaching after
    // the interruption must not wait for the owner to let go of it.
    let response = open_promptless_sse(&fixture.session).await;
    let read = read_sse_until(response, Duration::from_secs(5), |read| {
        has_event(&read.events, "RUN_FINISHED")
    })
    .await;
    assert!(
        has_event(&read.events, "RUN_FINISHED"),
        "a retained Cancel completes a new attachment: {:?}",
        read.events
    );
    assert!(fixture.session.lease.revalidate_ownership().await.unwrap());
    fixture.session.lease.release().await.unwrap();
}

/// A prompt sent after Ctrl+C, while the interrupted turn still awaits its
/// wind-up, is a turn of its own. The `Cancel` that is still the log's last
/// terminator belongs to the turn before it, and must not close this stream
/// the moment it opens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_typed_after_the_interrupt_is_not_finished_by_it() {
    let fixture = InterruptedRemote::new().await;
    fixture.interrupt().await;
    fixture.next_prompt().await;

    let response = open_promptless_sse(&fixture.session).await;
    let read = read_sse_for(response, Duration::from_secs(1)).await;
    assert!(
        has_event(&read.events, "RUN_STARTED"),
        "the follow must open: {:?}",
        read.events
    );
    assert!(
        !has_event(&read.events, "RUN_FINISHED"),
        "an earlier turn's Cancel must not finish this prompt's run: {:?}",
        read.events
    );
    fixture.session.lease.release().await.unwrap();
}

/// Live output belongs to the turn it was produced in. A chunk from the turn
/// the `Cancel` stopped carries a sequence below it, and the reader attached to
/// the prompt typed afterwards must never show it — only its own turn's output.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopped_turns_late_chunk_never_reaches_the_next_prompts_reader() {
    let fixture = InterruptedRemote::new().await;
    let stopped_seq = fixture
        .entries()
        .await
        .last()
        .map(|(seq, _)| *seq)
        .expect("the seeded prompt");
    fixture.interrupt().await;
    fixture.next_prompt().await;
    let current_seq = fixture
        .entries()
        .await
        .last()
        .map(|(seq, _)| *seq)
        .expect("the replacement prompt");

    // Attaching now reads the whole log, so the reader knows the Cancel's
    // sequence before either chunk arrives.
    let response = open_promptless_sse(&fixture.session).await;
    fixture
        .publish_chunk(stopped_seq, "late chunk from the stopped turn")
        .await;
    fixture
        .publish_chunk(current_seq, "output of the prompt typed after it")
        .await;

    let read = read_sse_until(response, Duration::from_secs(5), |read| {
        read.events
            .iter()
            .any(|event| event["delta"] == "output of the prompt typed after it")
    })
    .await;
    assert!(
        !read
            .events
            .iter()
            .any(|event| event["delta"] == "late chunk from the stopped turn"),
        "a chunk from below the Cancel must not be forwarded: {:?}",
        read.events
    );
    assert!(
        read.events
            .iter()
            .any(|event| event["delta"] == "output of the prompt typed after it"),
        "the current turn's own output must still reach the reader: {:?}",
        read.events
    );
    fixture.session.lease.release().await.unwrap();
}

fn storage_key(session: &LeasedSession) -> String {
    harnx_core::session_identity::session_key(Some("plain"), &session.session_id)
}
