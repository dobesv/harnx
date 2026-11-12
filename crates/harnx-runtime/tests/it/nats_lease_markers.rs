//! Nothing a lease leaves on its key lasts forever, and a lease taken over a
//! marker still expires on its own.
use crate::worker::require_nats_server;
use anyhow::{Context, Result};
use async_nats::header::NATS_MESSAGE_TTL;
use async_nats::jetstream;
use harnx_runtime::nats_lease::{
    ensure_lease_bucket, NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease,
};
use std::time::Duration;

fn config() -> NatsLeaseConfig {
    NatsLeaseConfig {
        replicas: 1,
        ..Default::default()
    }
}

async fn acquire(js: &jetstream::Context, session_id: &str) -> Result<NatsSessionLease> {
    NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream: js.clone(),
        session_id,
        worker_id: format!("{session_id}-worker"),
        generation: 1,
        config: config(),
        session_metadata: None,
    })
    .await?
    .context("acquire the session lease")
}

/// The `Nats-TTL` header on the latest message for `key`, lease or marker.
async fn ttl_of(js: &jetstream::Context, key: &str) -> Result<Option<String>> {
    let message = js
        .get_stream("KV_harnx_leases")
        .await?
        .get_last_raw_message_by_subject(&format!("$KV.harnx_leases.{key}"))
        .await
        .with_context(|| format!("read the latest message for {key}"))?;
    Ok(message
        .headers
        .get(NATS_MESSAGE_TTL)
        .map(|ttl| ttl.as_str().to_owned()))
}

fn seconds(duration: Duration) -> Option<String> {
    Some(duration.as_secs().to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn releasing_a_lease_leaves_a_marker_that_expires() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let js = jetstream::new(async_nats::connect(server.url()).await?);
    let lease = acquire(&js, "released").await?;
    let key = lease.key().to_owned();

    lease.release().await?;

    assert_eq!(ttl_of(&js, &key).await?, seconds(config().tombstone_ttl));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_lease_leaves_a_marker_that_expires() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let js = jetstream::new(async_nats::connect(server.url()).await?);
    let lease = acquire(&js, "dropped").await?;
    let key = lease.key().to_owned();

    drop(lease);

    // Drop releases from a spawned task; wait for its marker to land.
    tokio::time::timeout(Duration::from_secs(10), async {
        while ttl_of(&js, &key).await? == seconds(config().ttl) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        anyhow::Ok(())
    })
    .await??;
    assert_eq!(ttl_of(&js, &key).await?, seconds(config().tombstone_ttl));
    Ok(())
}

/// A lease taken after its key was released still carries the lease TTL, so
/// it expires if its holder dies before the first renewal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_taken_after_a_release_expires() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let js = jetstream::new(async_nats::connect(server.url()).await?);
    acquire(&js, "retaken").await?.release().await?;

    let lease = acquire(&js, "retaken").await?;

    assert_eq!(ttl_of(&js, lease.key()).await?, seconds(config().ttl));
    lease.release().await?;
    Ok(())
}

/// Older builds released leases with a plain delete, whose marker never
/// expires. A lease taken over one still carries the lease TTL.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_taken_over_a_permanent_marker_expires() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let js = jetstream::new(async_nats::connect(server.url()).await?);
    let bucket = ensure_lease_bucket(&js, &config()).await?;
    let key = config().key_for_session("old-marker");
    bucket.put(&key, "old lease".into()).await?;
    bucket.delete(&key).await?;
    assert_eq!(
        ttl_of(&js, &key).await?,
        None,
        "the old marker never expires"
    );

    let lease = acquire(&js, "old-marker").await?;

    assert_eq!(ttl_of(&js, lease.key()).await?, seconds(config().ttl));
    lease.release().await?;
    Ok(())
}

/// Deleting a session replaces whatever is on its lease key, a live lease or a
/// marker an older build left, with a marker that expires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_session_leaves_an_expiring_marker_on_its_lease_key() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let js = jetstream::new(async_nats::connect(server.url()).await?);
    let runtime = crate::worker::local_nats_runtime_config(server.url())
        .read()
        .clone();
    let agent = "lease-marker-agent";
    let storage_key = |id: &str| harnx_core::session_identity::session_key(Some(agent), id);

    let live = acquire(&js, &storage_key("deleted-live")).await?;
    live.stop_renewal_for_test().await;
    let bucket = ensure_lease_bucket(&js, &config()).await?;
    let old_key = config().key_for_session(&storage_key("deleted-old"));
    bucket.put(&old_key, "old lease".into()).await?;
    bucket.delete(&old_key).await?;

    for id in ["deleted-live", "deleted-old"] {
        harnx_runtime::nats_admin::delete_remote_session(&runtime, "local", agent, id).await?;
    }

    assert_eq!(
        ttl_of(&js, live.key()).await?,
        seconds(config().tombstone_ttl)
    );
    assert_eq!(
        ttl_of(&js, &old_key).await?,
        seconds(config().tombstone_ttl)
    );
    Ok(())
}
