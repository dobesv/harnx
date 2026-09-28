//! Plans KV bucket helper.

use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv};

/// KV bucket for plan documents.
pub const PLAN_BUCKET: &str = "harnx_plans";

/// Ensure the plans bucket exists.
pub async fn ensure_plans_bucket(
    jetstream: &jetstream::Context,
    _replicas: usize,
) -> Result<kv::Store> {
    match jetstream.get_key_value(PLAN_BUCKET).await {
        Ok(store) => Ok(store),
        Err(error) if error.kind() == jetstream::context::KeyValueErrorKind::GetBucket => jetstream
            .create_key_value(kv::Config {
                bucket: PLAN_BUCKET.to_string(),
                storage: jetstream::stream::StorageType::File,
                ..Default::default()
            })
            .await
            .context("failed to create plans bucket"),
        Err(error) => Err(error).context("failed to get plans bucket"),
    }
}

/// Get an existing plans bucket, or None if it doesn't exist.
pub async fn optional_plans_bucket(jetstream: &jetstream::Context) -> Result<Option<kv::Store>> {
    match jetstream.get_key_value(PLAN_BUCKET).await {
        Ok(store) => Ok(Some(store)),
        Err(error) if error.kind() == jetstream::context::KeyValueErrorKind::GetBucket => Ok(None),
        Err(error) => Err(error).context("failed to get plans bucket"),
    }
}

/// Delete all plan KV keys for an owner (prefix: `plan/<owner>/`).
pub async fn delete_plans_prefix(jetstream: &jetstream::Context, owner: &str) -> Result<usize> {
    use futures_util::StreamExt;

    let Some(store) = optional_plans_bucket(jetstream).await? else {
        return Ok(0);
    };

    let prefix = format!("plan/{}", owner);
    let mut keys = store.keys().await.map_err(anyhow::Error::from)?;
    let mut deleted = 0;

    while let Some(key) = keys.next().await {
        let key = key.map_err(anyhow::Error::from)?;
        if key == prefix || key.starts_with(&format!("{}/", prefix)) {
            store.purge(&key).await?;
            deleted += 1;
        }
    }

    Ok(deleted)
}
