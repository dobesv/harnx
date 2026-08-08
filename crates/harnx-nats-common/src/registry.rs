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
    if let Ok(store) = create {
        return Ok(store);
    }

    // The bucket already exists. It may predate TTL support, so reconcile it.
    if let Err(error) = reconcile_bucket_ttl(jetstream, bucket, ttl).await {
        // A read-only or permission-limited connection can still use the
        // bucket; losing expiry is a degradation, not a failure to start.
        log::warn!("could not set max_age on bucket '{bucket}': {error:#}");
    }

    jetstream
        .get_key_value(bucket)
        .await
        .map_err(anyhow::Error::from)
        .with_context(|| format!("open KV bucket '{bucket}'"))
}

async fn reconcile_bucket_ttl(
    jetstream: &jetstream::Context,
    bucket: &str,
    ttl: Duration,
) -> Result<()> {
    let stream_name = format!("KV_{bucket}");
    let mut config = jetstream
        .get_stream(&stream_name)
        .await
        .with_context(|| format!("get stream '{stream_name}'"))?
        .info()
        .await
        .with_context(|| format!("stream info for '{stream_name}'"))?
        .config
        .clone();
    if config.max_age == ttl {
        return Ok(());
    }
    config.max_age = ttl;
    // A bucket created before TTL support (or with a longer TTL) can carry a
    // duplicate window wider than the new max_age; the server rejects that
    // combination outright, so it has to shrink along with max_age.
    if config.duplicate_window > ttl {
        config.duplicate_window = ttl;
    }
    jetstream
        .update_stream(config)
        .await
        .with_context(|| format!("set max_age on '{stream_name}'"))?;
    Ok(())
}
