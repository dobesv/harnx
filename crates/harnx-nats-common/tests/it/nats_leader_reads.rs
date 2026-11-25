//! Fidelity of leader-read KV metadata: `entry_with_headers` answers with
//! the entry and the headers of one stored message, so a caller can judge
//! one revision's payload, revision, timestamp and headers together.
use crate::registry_ttl::{spawn_nats_server, test_jetstream, NatsServerHandle};
use anyhow::{Context, Result};
use async_nats::header::{NATS_EXPECTED_LAST_SUBJECT_SEQUENCE, NATS_MESSAGE_TTL};
use async_nats::jetstream::{self, kv, stream::StorageType};
use harnx_nats_common::leader_reads::{entry, entry_with_headers, get, EntryWithHeaders};
use std::time::Duration;

/// The header a KV store writes on the markers that delete and purge a key;
/// the leader-read decoder reads it, and the assertions here check it in the
/// decoded `HeaderMap`.
const KV_OPERATION: &str = "KV-Operation";

/// A KV store standing in for the lease bucket: created the way
/// `ensure_lease_bucket` creates it, with `limit_markers` set, because a
/// per-message `Nats-TTL` is refused on a stream that doesn't allow one, and
/// no bucket TTL, so records expire per message, not per bucket.
async fn probe_store(jetstream: &jetstream::Context, bucket: &str) -> Result<kv::Store> {
    jetstream
        .create_key_value(kv::Config {
            bucket: bucket.to_string(),
            history: 1,
            limit_markers: Some(Duration::from_secs(30)),
            num_replicas: 1,
            max_age: Duration::ZERO,
            storage: StorageType::File,
            ..Default::default()
        })
        .await
        .with_context(|| format!("create KV bucket {bucket}"))
}

/// The broker, its JetStream context and a lease-like KV store in `bucket`,
/// or `None` when no `nats-server` binary is available and the test must
/// skip. The broker handle must stay alive for as long as the test reads.
async fn probe_fixture(
    bucket: &str,
) -> Result<Option<(NatsServerHandle, jetstream::Context, kv::Store)>> {
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: no nats-server binary available");
        return Ok(None);
    };
    let jetstream = test_jetstream(&server).await?;
    let store = probe_store(&jetstream, bucket).await?;
    Ok(Some((server, jetstream, store)))
}

/// Publish `value` under `key` with `Nats-TTL` seconds, the way
/// `nats_lease::slot::LeaseSlot` writes a lease: a direct stream publish,
/// not a `kv::Store` put. Returns the stream sequence the server assigned.
async fn publish_ttl_record(
    jetstream: &jetstream::Context,
    bucket: &str,
    key: &str,
    ttl_secs: u64,
    value: &[u8],
) -> Result<u64> {
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(NATS_MESSAGE_TTL, ttl_secs.to_string());
    headers.insert(NATS_EXPECTED_LAST_SUBJECT_SEQUENCE, 0.to_string());
    let subject = format!("$KV.{bucket}.{key}");
    let ack = jetstream
        .publish_with_headers(subject, headers, value.to_vec().into())
        .await
        .context("publish TTL record")?
        .await
        .context("await TTL record publish")?;
    Ok(ack.sequence)
}

/// The broker, the store and the leader-read answer for `key` once a value
/// is written and then removed with `marker` (a delete or a purge), so the
/// latest revision is the marker itself. `None` when no `nats-server` binary
/// is available and the test must skip.
async fn marker_fixture(
    bucket: &str,
    key: &str,
    marker: kv::Operation,
) -> Result<Option<(NatsServerHandle, kv::Store, EntryWithHeaders)>> {
    let Some((server, _jetstream, store)) = probe_fixture(bucket).await? else {
        return Ok(None);
    };
    store
        .put(key, "holder record".into())
        .await
        .context("seed KV put")?;
    match marker {
        kv::Operation::Delete => store.delete(key).await.context("KV delete")?,
        kv::Operation::Purge => store.purge(key).await.context("KV purge")?,
        kv::Operation::Put => unreachable!("only markers are seeded here"),
    }
    let read = entry_with_headers(&store, key)
        .await
        .context("leader-read entry with headers")?
        .expect("a marker is still a revision");
    Ok(Some((server, store, read)))
}

#[tokio::test]
async fn put_with_headers_answers_entry_and_headers_of_the_same_message() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream, store)) = probe_fixture("leader_ttl_put").await? else {
        return Ok(());
    };
    let key = "worker/session/lease";
    let payload: &[u8] = b"holder record";

    let revision = publish_ttl_record(&jetstream, "leader_ttl_put", key, 30, payload).await?;
    let read = entry_with_headers(&store, key)
        .await
        .context("leader-read entry with headers")?
        .expect("stored key must answer");

    // The entry is the stored message: operation, revision, value and key.
    assert_eq!(read.entry.operation, kv::Operation::Put);
    assert_eq!(read.entry.key, key);
    assert_eq!(read.entry.revision, revision);
    assert_eq!(read.entry.value.as_ref(), payload);
    assert_eq!(read.entry.bucket, "leader_ttl_put");

    // The headers are the same message's, undecoded: the writer's TTL, the
    // publish's expected revision, and no KV operation marker.
    assert_eq!(read.headers.get(NATS_MESSAGE_TTL).unwrap().as_str(), "30");
    assert_eq!(
        read.headers
            .get(NATS_EXPECTED_LAST_SUBJECT_SEQUENCE)
            .unwrap()
            .as_str(),
        "0"
    );
    assert_eq!(read.headers.get(KV_OPERATION), None);

    // The timestamp is the stored message's broker time, not the read's.
    let raw = store
        .stream
        .get_last_raw_message_by_subject(&format!("$KV.leader_ttl_put.{key}"))
        .await
        .context("raw leader read")?;
    assert_eq!(read.entry.created, raw.time);

    // The record-only reader still answers the same revision.
    let record = entry(&store, key).await?.expect("record-only read");
    assert_eq!(record.revision, revision);
    assert_eq!(record.created, raw.time);
    assert_eq!(record.value.as_ref(), payload);
    Ok(())
}

#[tokio::test]
async fn headerless_put_answers_put_entry_without_ttl_header() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, _jetstream, store)) = probe_fixture("leader_headerless").await? else {
        return Ok(());
    };
    let key = "legacy/lease";

    let revision = store
        .put(key, "legacy record".into())
        .await
        .context("headerless KV put")?;
    let read = entry_with_headers(&store, key)
        .await
        .context("leader-read entry with headers")?
        .expect("stored key must answer");

    assert_eq!(read.entry.operation, kv::Operation::Put);
    assert_eq!(read.entry.revision, revision);
    assert_eq!(read.entry.value.as_ref(), b"legacy record");
    // A KV put writes no headers at all: the map is empty, and in particular
    // carries no `Nats-TTL` for a caller to mistake for a real one.
    assert_eq!(read.headers.get(NATS_MESSAGE_TTL), None);
    assert_eq!(read.headers.get(KV_OPERATION), None);

    assert_eq!(
        get(&store, key).await?.as_deref(),
        Some(b"legacy record".as_ref())
    );
    Ok(())
}

#[tokio::test]
async fn delete_marker_keeps_marker_operation_and_headers() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, store, read)) = marker_fixture(
        "leader_delete_marker",
        "worker/session/lease",
        kv::Operation::Delete,
    )
    .await?
    else {
        return Ok(());
    };

    // A marker decodes to its operation and retains its decoded marker
    // header, instead of passing as a value-bearing put.
    assert_eq!(read.entry.operation, kv::Operation::Delete);
    assert!(read.headers.get(KV_OPERATION).is_some());
    // The existing readers keep their contract over a marker: `get` hides
    // the key, `entry` still reports the revision.
    assert_eq!(get(&store, "worker/session/lease").await?, None);
    let marker = entry(&store, "worker/session/lease")
        .await?
        .expect("record-only read");
    assert_eq!(marker.operation, kv::Operation::Delete);
    assert_eq!(marker.revision, read.entry.revision);
    Ok(())
}

#[tokio::test]
async fn purge_marker_keeps_marker_operation_and_headers() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, store, read)) = marker_fixture(
        "leader_purge_marker",
        "worker/session/lease",
        kv::Operation::Purge,
    )
    .await?
    else {
        return Ok(());
    };

    assert_eq!(read.entry.operation, kv::Operation::Purge);
    assert!(read.headers.get(KV_OPERATION).is_some());
    assert_eq!(get(&store, "worker/session/lease").await?, None);
    Ok(())
}

#[tokio::test]
async fn absent_key_answers_none_and_invalid_key_fails() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, _jetstream, store)) = probe_fixture("leader_absent").await? else {
        return Ok(());
    };

    // Absent means absent, with or without headers in the answer.
    assert!(entry_with_headers(&store, "never/written").await?.is_none());
    assert!(entry(&store, "never/written").await?.is_none());
    assert!(get(&store, "never/written").await?.is_none());

    // A wildcard would make the leader answer with another key's revision,
    // so it stays refused exactly as before.
    assert!(entry_with_headers(&store, "wild*card").await.is_err());
    assert!(entry_with_headers(&store, "trailing.").await.is_err());
    Ok(())
}
