//! Deleting a session leaves only markers that expire in `harnx_sessions`.
use crate::worker::{local_nats_runtime_config, require_nats_server};
use anyhow::{Context, Result};
use async_nats::header::NATS_MESSAGE_TTL;
use async_nats::jetstream::{self, kv, stream};
use harnx_runtime::nats_session_metadata::{
    activity_key, metadata_key, SessionInitializer, SessionMetadata, SessionMetadataStore,
    SESSION_METADATA_BUCKET,
};
use std::time::Duration;

const AGENT: &str = "deletion-marker-agent";

/// The `Nats-TTL` header on the latest message for `key` in the metadata
/// bucket.
async fn ttl_of(js: &jetstream::Context, key: &str) -> Result<Option<String>> {
    let message = js
        .get_stream(format!("KV_{SESSION_METADATA_BUCKET}"))
        .await?
        .get_last_raw_message_by_subject(&format!("$KV.{SESSION_METADATA_BUCKET}.{key}"))
        .await
        .with_context(|| format!("read the latest message for {key}"))?;
    Ok(message
        .headers
        .get(NATS_MESSAGE_TTL)
        .map(|ttl| ttl.as_str().to_owned()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_session_leaves_expiring_metadata_markers() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let js = jetstream::new(async_nats::connect(server.url()).await?);
    let store = SessionMetadataStore::ensure(&js, 1).await?;
    let metadata = SessionMetadata::new(
        "deleted",
        SessionInitializer::named(AGENT, Default::default()),
    );
    store.create(&metadata).await?;
    let key = metadata.storage_key();
    let runtime = local_nats_runtime_config(server.url()).read().clone();

    harnx_runtime::nats_admin::delete_remote_session(&runtime, "local", AGENT, "deleted").await?;

    let hour = Some(
        harnx_nats_common::markers::DELETION_MARKER_TTL
            .as_secs()
            .to_string(),
    );
    assert!(store.get(&key).await?.is_none());
    assert_eq!(ttl_of(&js, &metadata_key(&key)).await?, hour);
    assert_eq!(ttl_of(&js, &activity_key(&key)).await?, hour);
    Ok(())
}

/// Builds before this created the bucket without per-message TTLs. Opening the
/// metadata store turns them on, so deletions there leave markers that expire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opening_an_older_metadata_bucket_allows_expiring_markers() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let js = jetstream::new(async_nats::connect(server.url()).await?);
    js.create_key_value(kv::Config {
        bucket: SESSION_METADATA_BUCKET.to_string(),
        history: 1,
        num_replicas: 1,
        storage: stream::StorageType::File,
        ..Default::default()
    })
    .await
    .context("create the bucket the way older builds did")?;

    SessionMetadataStore::ensure(&js, 1).await?;

    let config = js
        .get_stream(format!("KV_{SESSION_METADATA_BUCKET}"))
        .await?
        .info()
        .await?
        .config
        .clone();
    assert!(config.allow_message_ttl);
    assert_eq!(
        config.subject_delete_marker_ttl,
        Some(Duration::from_secs(60))
    );
    Ok(())
}
