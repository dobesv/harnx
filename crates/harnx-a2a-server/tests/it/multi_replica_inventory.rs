//! Broker capability and retention evidence for later coordination work.
use crate::support::{Broker, DEADLINE};
use anyhow::{Context, Result};
use async_nats::{
    header::{NATS_EXPECTED_LAST_SUBJECT_SEQUENCE, NATS_MESSAGE_ID, NATS_MESSAGE_TTL},
    jetstream::stream,
};
use harnx_runtime::nats_session_metadata::{SessionMetadataStore, SESSION_METADATA_BUCKET};
use std::time::Duration;

#[tokio::test]
async fn broker_supports_conditional_publish_dedup_ttl_and_capacity_rejection() -> Result<()> {
    harnx_core::require_nextest();
    let (_broker, _, client) = Broker::start().await?;
    assert_eq!(client.server_info().max_payload, 1_048_576);
    assert_eq!(client.server_info().version, "2.11.6");
    let js = async_nats::jetstream::new(client);
    let mut probe = js
        .create_stream(stream::Config {
            name: "A2A_CAPABILITY_PROBE".into(),
            subjects: vec!["a2a.probe.>".into()],
            retention: stream::RetentionPolicy::Limits,
            max_bytes: 4096,
            max_messages_per_subject: 1,
            discard: stream::DiscardPolicy::New,
            discard_new_per_subject: true,
            allow_message_ttl: true,
            ..Default::default()
        })
        .await?;
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(NATS_EXPECTED_LAST_SUBJECT_SEQUENCE, "0");
    headers.insert(NATS_MESSAGE_ID, "stable-commit");
    headers.insert(NATS_MESSAGE_TTL, "30s");
    assert_publication_capabilities(&js, &mut probe, headers).await?;
    // An explicit byte budget also rejects oversized events on a fresh subject.
    assert!(js
        .publish("a2a.probe.other", vec![b'x'; 4097].into())
        .await?
        .await
        .is_err());
    js.delete_stream("A2A_CAPABILITY_PROBE").await?;
    Ok(())
}

#[tokio::test]
async fn metadata_has_no_automatic_expiry_and_session_gc_purges_a2a_prefix_only() -> Result<()> {
    harnx_core::require_nextest();
    let (_broker, _, client) = Broker::start().await?;
    let js = async_nats::jetstream::new(client);
    let metadata = SessionMetadataStore::ensure(&js, 1).await?;
    let kv = js.get_key_value(SESSION_METADATA_BUCKET).await?;
    let mut metadata_stream = js
        .get_stream(format!("KV_{SESSION_METADATA_BUCKET}"))
        .await?;
    let config = &metadata_stream.info().await?.config;
    assert_eq!(config.max_age, Duration::ZERO);
    assert_eq!(config.max_messages_per_subject, 1);
    assert!(
        config.max_bytes <= 0,
        "no finite production KV storage budget yet"
    );
    assert!(
        config.max_message_size <= 0,
        "server max_payload bounds values"
    );
    assert_eq!(config.num_replicas, 1);
    let keys = [
        "sessions/gc-target/meta",
        "sessions/gc-target/a2a/context",
        "sessions/gc-target/a2a/messages/id",
        "sessions/gc-target/a2a/tasks/id",
    ];
    for key in keys {
        kv.put(key, "{}".into()).await?;
    }
    let sibling = "sessions/gc-target-other/a2a/context";
    kv.put(sibling, "{}".into()).await?;
    assert_eq!(
        metadata.purge_session_prefix("gc-target").await?,
        keys.len()
    );
    for key in keys {
        assert!(kv.get(key).await?.is_none());
    }
    assert!(
        kv.get(sibling).await?.is_some(),
        "prefix boundary preserves other sessions"
    );
    Ok(())
}

#[tokio::test]
async fn restricted_replica_can_bootstrap_metadata_cas_and_leader_read_but_not_other_subjects(
) -> Result<()> {
    harnx_core::require_nextest();
    tokio::time::timeout(DEADLINE, restricted_replica())
        .await
        .context("restricted permission deadline")?
}
async fn restricted_replica() -> Result<()> {
    let config = r#"
no_auth_user: admin
 authorization {
   users: [
     {user: admin},
     {user: replica, password: test, permissions: {
       publish: {allow: ["$JS.API.INFO", "$JS.API.STREAM.>", "$JS.API.CONSUMER.>", "$JS.ACK.>", "$KV.harnx_sessions.>"]},
       subscribe: {allow: ["_INBOX.>"]}
     }}
   ]
 }
"#;
    let (_broker, url, _) = Broker::start_with_config(config).await?;
    let (errors, mut received) = tokio::sync::mpsc::unbounded_channel();
    let client = async_nats::ConnectOptions::new()
        .user_and_password("replica".into(), "test".into())
        .event_callback(move |event| {
            let errors = errors.clone();
            async move {
                if let async_nats::Event::ServerError(error) = event {
                    let _ = errors.send(error.to_string());
                }
            }
        })
        .connect(url)
        .await?;
    let js = async_nats::jetstream::new(client.clone());
    assert_restricted_metadata(&js).await?;
    assert!(
        received.try_recv().is_err(),
        "no hidden permission error during bootstrap/read"
    );
    client
        .publish("unapproved.subject", "denied".into())
        .await?;
    client.flush().await?;
    let error = received.recv().await.context("permission event")?;
    assert!(error.contains("Permissions Violation"), "{error}");
    assert!(error.contains("unapproved.subject"), "{error}");
    Ok(())
}

async fn assert_publication_capabilities(
    js: &async_nats::jetstream::Context,
    probe: &mut stream::Stream,
    mut headers: async_nats::HeaderMap,
) -> Result<()> {
    let first = js
        .publish_with_headers("a2a.probe.task", headers.clone(), "event".into())
        .await?
        .await?;
    let retry = js
        .publish_with_headers("a2a.probe.task", headers.clone(), "event".into())
        .await?
        .await?;
    assert!(retry.duplicate);
    assert_eq!(retry.sequence, first.sequence);
    headers.insert(NATS_MESSAGE_ID, "different-commit");
    let stale = js
        .publish_with_headers("a2a.probe.task", headers.clone(), "event".into())
        .await?
        .await;
    assert_eq!(
        stale.expect_err("stale publish accepted").kind(),
        async_nats::jetstream::context::PublishErrorKind::WrongLastSequence
    );
    headers.insert(
        NATS_EXPECTED_LAST_SUBJECT_SEQUENCE,
        first.sequence.to_string(),
    );
    let full = js
        .publish_with_headers("a2a.probe.task", headers, "event".into())
        .await?
        .await;
    assert!(
        full.is_err(),
        "per-subject DiscardNew rejects instead of evicting predecessor"
    );
    assert_eq!(probe.info().await?.state.messages, 1);
    assert_eq!(
        probe
            .get_last_raw_message_by_subject("a2a.probe.task")
            .await?
            .sequence,
        first.sequence
    );
    Ok(())
}

async fn assert_restricted_metadata(js: &async_nats::jetstream::Context) -> Result<()> {
    let metadata = SessionMetadataStore::ensure(js, 1).await?;
    let kv = js.get_key_value(SESSION_METADATA_BUCKET).await?;
    let revision = kv
        .create(
            "sessions/permission/a2a/index",
            serde_json::to_vec(&harnx_runtime::nats_session_metadata::TaskIndex::default())?.into(),
        )
        .await?;
    let index_bytes =
        serde_json::to_vec(&harnx_runtime::nats_session_metadata::TaskIndex::default())?;
    let updated = kv
        .update(
            "sessions/permission/a2a/index",
            index_bytes.clone().into(),
            revision,
        )
        .await?;
    assert!(
        kv.update(
            "sessions/permission/a2a/index",
            index_bytes.into(),
            revision
        )
        .await
        .is_err(),
        "stale metadata CAS must fail"
    );
    let (_, read_revision) = metadata
        .get_a2a_task_index("permission")
        .await?
        .context("leader index read")?;
    assert_eq!(
        read_revision, updated,
        "real leader read succeeds under restricted API permissions"
    );
    Ok(())
}
