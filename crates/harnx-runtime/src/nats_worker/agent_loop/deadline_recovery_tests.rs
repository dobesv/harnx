use super::*;
use crate::nats_session_metadata::{SessionInitializer, SessionMetadata, SessionMetadataStore};
use crate::nats_test_common::spawn_nats_server;
use crate::nats_worker::backend::test_session_authority;
use futures_util::StreamExt;
use harnx_core::message::{MessageContent, MessageRole};
use std::time::Duration;

/// Every activation runs this recovery before its turn starts, and most have
/// no unanswered tool call to recover. Opening the journal sends the server a
/// request to create its bucket, so an activation with nothing to look up
/// leaves it closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_activation_with_no_orphan_calls_leaves_the_journal_closed() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let client = async_nats::connect(server.url()).await.unwrap();
    let jetstream = async_nats::jetstream::new(client.clone());
    let metadata = SessionMetadataStore::ensure(&jetstream, 1).await.unwrap();
    let session_id = crate::nats_worker::new_remote_session_id();
    metadata
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("metis", Default::default()),
        ))
        .await
        .unwrap();
    let storage_key = harnx_core::session_identity::session_key(Some("metis"), &session_id);
    crate::nats_session_log::NatsSessionLog::new_with_replicas(
        jetstream.clone(),
        storage_key.clone(),
        1,
    )
    .append_event_async(&SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("what time is it".into()),
        timestamp: None,
        fence_token: None,
    })
    .await
    .unwrap();
    let backend = NatsSessionLogBackend::new(jetstream.clone(), &storage_key, 1);
    let lease = test_session_authority(&jetstream, &storage_key, &metadata).await;
    let config: GlobalConfig = Arc::new(crate::config::ConfigLock::new(Default::default()));
    let mut bucket_creates = client
        .subscribe(format!(
            "$JS.API.STREAM.CREATE.KV_{}",
            harnx_toolset_server::invocation_journal::BUCKET
        ))
        .await
        .unwrap();
    // The server handles one connection's messages in order, so a round trip
    // on the subscription's connection registers it before the recovery runs
    // and, afterwards, has every request the recovery made queued to it.
    jetstream.query_account().await.unwrap();

    recover_completed_before_deadline(&backend, &lease, &config, &jetstream, 1)
        .await
        .unwrap();

    jetstream.query_account().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), bucket_creates.next())
            .await
            .is_err(),
        "the journal was opened with nothing to look up"
    );
}
