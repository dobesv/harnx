use super::*;
use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent};
use harnx_execution_control::{ExecutionStore, InterruptScope, OperationRef, Owner};
use harnx_runtime::nats_event_sink::{events_subject, AdvisoryEnvelope};

struct GatedRemote {
    session: LeasedSession,
    store: ExecutionStore,
    original: OperationRef,
}

impl GatedRemote {
    async fn new() -> Self {
        let mut session = seed_in_progress_leased_session()
            .await
            .expect("NATS required");
        let store = ExecutionStore::ensure(&session.jetstream, 1).await.unwrap();
        let original = store
            .session(&storage_key(&session), None, Some("g1"))
            .await
            .unwrap()
            .reference;
        session.lease.release().await.unwrap();
        session.lease = Arc::new(bound_lease(&session, "g1").await);
        let log = NatsSessionLog::new(session.jetstream.clone(), storage_key(&session));
        for (seq, entry) in log.load_events_async().await.unwrap() {
            if let SessionLogEntry::Message {
                id: Some(id),
                role: MessageRole::User,
                ..
            } = entry
            {
                store.reserve_prompt(&original, &id).await.unwrap();
                store.commit_prompt(&original, &id, seq).await.unwrap();
            }
        }
        claim_generation(&store, &original, &session.lease).await;
        Self {
            session,
            store,
            original,
        }
    }

    async fn stop(&self) {
        let context = self.store.activate_gate(&self.original).await.unwrap();
        self.store
            .interrupt(
                &InterruptScope {
                    gate_root: context.gate_root().clone(),
                    operation: self.original.clone(),
                    reason: "remote frontend interrupt".into(),
                },
                "stop-g1",
            )
            .await
            .unwrap();
    }

    async fn replace(&self) -> NatsSessionLease {
        let next = self
            .store
            .session(&storage_key(&self.session), None, Some("g2"))
            .await
            .unwrap();
        let lease = bound_lease(&self.session, "g2").await;
        self.store
            .reserve_prompt(&next.reference, "g2-input")
            .await
            .unwrap();
        let seq = NatsSessionLog::new(self.session.jetstream.clone(), storage_key(&self.session))
            .append_event_async(&SessionLogEntry::Message {
                id: Some("g2-input".into()),
                role: MessageRole::User,
                content: MessageContent::Text("replacement prompt".into()),
                timestamp: None,
                fence_token: None,
            })
            .await
            .unwrap();
        self.store
            .commit_prompt(&next.reference, "g2-input", seq)
            .await
            .unwrap();
        claim_generation(&self.store, &next.reference, &lease).await;
        lease
    }

    async fn publish(&self, generation: &str, text: &str) {
        let client = self
            .session
            .config
            .nats_client(LOCAL_CLUSTER_KEY)
            .await
            .unwrap();
        let envelope = AdvisoryEnvelope::new(
            u64::MAX,
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text(text.into())],
            }),
        )
        .with_execution_id(generation);
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

async fn bound_lease(session: &LeasedSession, execution: &str) -> NatsSessionLease {
    NatsSessionLease::acquire_for_execution(
        NatsLeaseAcquireParams {
            jetstream: session.jetstream.clone(),
            session_id: &storage_key(session),
            worker_id: format!("remote-{execution}"),
            generation: 1,
            config: NatsLeaseConfig {
                ttl: Duration::from_secs(300),
                renew_interval: Duration::from_secs(90),
                ..Default::default()
            },
            session_metadata: None,
        },
        Some(execution.into()),
    )
    .await
    .unwrap()
    .expect("generation lease")
}

async fn claim_generation(
    store: &ExecutionStore,
    reference: &OperationRef,
    lease: &NatsSessionLease,
) {
    store
        .claim(
            reference,
            Owner {
                instance_id: lease.worker_id().into(),
                fence: lease.fence_token(),
            },
        )
        .await
        .unwrap();
    store.activate_gate(reference).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupted_remote_run_finishes_while_g2_holds_lease_without_turn_end() {
    let fixture = GatedRemote::new().await;
    let response = open_promptless_sse(&fixture.session).await;
    fixture.stop().await;
    let replacement = fixture.replace().await;
    // No transcript Cancel/TurnEnd and no lease-free interval visible to this reader.
    fixture.publish("g2", "must not enter g1 run").await;
    fixture.publish("g1", "late stopped chunk").await;
    let read = read_sse_until(response, Duration::from_secs(3), |read| {
        has_event(&read.events, "RUN_FINISHED")
    })
    .await;
    assert!(
        has_event(&read.events, "RUN_FINISHED"),
        "root acceptance must end remote follow: {:?}",
        read.events
    );
    assert!(
        !has_event(&read.events, "TEXT_MESSAGE_CONTENT"),
        "a stopped run must not forward either generation: {:?}",
        read.events
    );
    assert!(replacement.revalidate_ownership().await.unwrap());
    assert!(fixture
        .store
        .accepted_stop(&fixture.original)
        .await
        .unwrap()
        .is_some());
    replacement.release().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_attach_after_acceptance_does_not_wait_for_original_lease_cleanup() {
    let fixture = GatedRemote::new().await;
    fixture.stop().await;
    let response = open_promptless_sse(&fixture.session).await;
    let read = read_sse_until(response, Duration::from_secs(3), |read| {
        has_event(&read.events, "RUN_FINISHED")
    })
    .await;
    assert!(
        has_event(&read.events, "RUN_FINISHED"),
        "retained root stop completes a new attachment"
    );
    assert!(fixture.session.lease.revalidate_ownership().await.unwrap());
    fixture.session.lease.release().await.unwrap();
}

fn storage_key(session: &LeasedSession) -> String {
    harnx_core::session_identity::session_key(Some("plain"), &session.session_id)
}
