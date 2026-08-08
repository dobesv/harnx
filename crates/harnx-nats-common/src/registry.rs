use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv, stream};
use std::time::Duration;

/// How long a registration survives without a refresh.
///
/// Three times the publisher's refresh interval, so two missed refreshes are
/// tolerated before a server is treated as gone.
pub const REGISTRATION_TTL: Duration = Duration::from_secs(90);

/// Open a KV bucket, creating it with `ttl` or bringing an existing bucket up to
/// that TTL.
pub async fn ensure_bucket_with_ttl(
    jetstream: &jetstream::Context,
    bucket: &str,
    ttl: Duration,
    num_replicas: usize,
) -> Result<kv::Store> {
    let create = jetstream
        .create_key_value(kv::Config {
            bucket: bucket.to_string(),
            history: 1,
            num_replicas,
            max_age: ttl,
            storage: stream::StorageType::File,
            ..Default::default()
        })
        .await;
    let create_error = match create {
        Ok(store) => return Ok(store),
        Err(error) => error,
    };
    // The usual case here is "bucket already exists"; anything else is worth
    // keeping around in case the fallback below also fails and needs
    // explaining.
    log::debug!("create_key_value for bucket '{bucket}' did not succeed: {create_error:#}");

    // The bucket already exists. It may predate TTL support, or its replica
    // count may no longer match this config (an operator raised `replicas`
    // after the bucket was first created), so reconcile both.
    if let Err(error) = reconcile_bucket_config(jetstream, bucket, ttl, num_replicas).await {
        // A read-only or permission-limited connection can still use the
        // bucket; losing expiry or the intended replica count is a
        // degradation, not a failure to start. Raising replicas above what
        // the cluster can support (e.g. `replicas: 3` against a single-node
        // dev server) fails the same way and must not stop harnx starting.
        log::warn!("could not reconcile config for bucket '{bucket}': {error:#}");
    }

    jetstream
        .get_key_value(bucket)
        .await
        .map_err(anyhow::Error::from)
        .with_context(|| format!("open KV bucket '{bucket}'"))
}

/// Raise (or lower) just the replica count on an existing bucket's stream,
/// leaving every other stream setting untouched. A no-op, reported as `Ok`,
/// when the count already matches.
///
/// For buckets whose expiry isn't TTL-based (`harnx_leases` uses tombstone
/// limit markers instead of `max_age`; `harnx_sessions` has no expiry at
/// all) and so don't go through [`ensure_bucket_with_ttl`], but still need
/// the same "an operator raised `replicas` after this bucket already
/// existed" reconcile that `ensure_bucket_with_ttl` gives TTL-based buckets.
pub async fn reconcile_bucket_replicas(
    jetstream: &jetstream::Context,
    bucket: &str,
    num_replicas: usize,
) -> Result<()> {
    let stream_name = format!("KV_{bucket}");
    let mut config = fetch_stream_config(jetstream, &stream_name).await?;
    if config.num_replicas == num_replicas {
        return Ok(());
    }
    let current_replicas = config.num_replicas;
    config.num_replicas = num_replicas;
    update_stream_reporting_replicas(jetstream, &stream_name, config, current_replicas).await
}

/// Bring an existing bucket's stream up to the requested TTL and replica
/// count, in one `update_stream` call. Both drift independently of bucket
/// creation: TTL predates this reconcile path, and replicas can be raised
/// later by an operator editing cluster config after the bucket already
/// exists (`num_replicas` is otherwise fixed at creation time).
async fn reconcile_bucket_config(
    jetstream: &jetstream::Context,
    bucket: &str,
    ttl: Duration,
    num_replicas: usize,
) -> Result<()> {
    let stream_name = format!("KV_{bucket}");
    let mut config = fetch_stream_config(jetstream, &stream_name).await?;
    let current_replicas = config.num_replicas;
    if config.max_age == ttl && current_replicas == num_replicas {
        return Ok(());
    }
    config.max_age = ttl;
    // A bucket created before TTL support (or with a longer TTL) can carry a
    // duplicate window wider than the new max_age; the server rejects that
    // combination outright, so it has to shrink along with max_age.
    if config.duplicate_window > ttl {
        config.duplicate_window = ttl;
    }
    config.num_replicas = num_replicas;
    update_stream_reporting_replicas(jetstream, &stream_name, config, current_replicas).await
}

async fn fetch_stream_config(
    jetstream: &jetstream::Context,
    stream_name: &str,
) -> Result<stream::Config> {
    Ok(jetstream
        .get_stream(stream_name)
        .await
        .with_context(|| format!("get stream '{stream_name}'"))?
        .info()
        .await
        .with_context(|| format!("stream info for '{stream_name}'"))?
        .config
        .clone())
}

/// Apply a stream config change that includes a replica-count change.
/// Changing replicas is a Raft peer-set change, not a metadata tweak: the
/// server rejects it outright when the cluster can't support the new count
/// (there's nothing to retry or wait for), so this is one attempt, reported
/// by the caller rather than retried here.
async fn update_stream_reporting_replicas(
    jetstream: &jetstream::Context,
    stream_name: &str,
    config: stream::Config,
    current_replicas: usize,
) -> Result<()> {
    let requested_replicas = config.num_replicas;
    jetstream.update_stream(config).await.with_context(|| {
        format!(
            "update '{stream_name}' (replicas currently {current_replicas}, requested {requested_replicas})"
        )
    })?;
    Ok(())
}
