//! Durable per-activation failure accounting.
//!
//! JetStream's delivery count includes busy and shutdown redeliveries, so it
//! cannot decide when an activation has exhausted its failure budget. This KV
//! counter advances only when worker admission or session setup actually fails.

use anyhow::{Context, Result};
use async_nats::jetstream::kv;
use std::time::Duration;

const ACTIVATION_FAILURE_BUCKET: &str = "harnx_activation_failures";
const ACTIVATION_FAILURE_TTL: Duration = Duration::from_secs(60 * 60);
const CAS_RETRY_LIMIT: usize = 16;

#[derive(Clone)]
pub(super) struct ActivationFailureTracker {
    store: kv::Store,
}

impl ActivationFailureTracker {
    pub(super) async fn ensure(
        jetstream: &async_nats::jetstream::Context,
        replicas: usize,
    ) -> Result<Self> {
        let store = harnx_nats_common::registry::ensure_bucket_with_ttl(
            jetstream,
            ACTIVATION_FAILURE_BUCKET,
            ACTIVATION_FAILURE_TTL,
            replicas,
        )
        .await
        .context("ensure activation failure counter bucket")?;
        Ok(Self { store })
    }

    pub(super) async fn increment(&self, key: &str) -> Result<u64> {
        for attempt in 0..CAS_RETRY_LIMIT {
            let (count, revision) = self.snapshot(key).await?;
            let next = count.saturating_add(1);
            match self.write(key, next, revision).await {
                Ok(_) => return Ok(next),
                Err(error) if is_cas_conflict(&error) && attempt + 1 < CAS_RETRY_LIMIT => {
                    tokio::task::yield_now().await;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("increment activation failure counter '{key}'"))
                }
            }
        }
        unreachable!("bounded CAS loop always returns")
    }

    pub(super) async fn clear(&self, key: &str) -> Result<()> {
        self.store
            .delete(key)
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("clear activation failure counter '{key}'"))
    }

    async fn snapshot(&self, key: &str) -> Result<(u64, u64)> {
        // Another worker may have counted the previous failure a moment ago. A
        // follower that hasn't applied it would hand every retry of the CAS
        // below the same stale revision.
        let Some(entry) = harnx_nats_common::leader_reads::entry(&self.store, key).await? else {
            return Ok((0, 0));
        };
        if !matches!(entry.operation, kv::Operation::Put) {
            return Ok((0, 0));
        }
        let count = std::str::from_utf8(&entry.value)
            .context("activation failure counter is not UTF-8")?
            .parse::<u64>()
            .context("activation failure counter is not an integer")?;
        Ok((count, entry.revision))
    }

    async fn write(&self, key: &str, count: u64, revision: u64) -> Result<u64> {
        let value = count.to_string().into();
        if revision == 0 {
            return self
                .store
                .create(key, value)
                .await
                .map_err(anyhow::Error::from);
        }
        harnx_nats_common::cas::update(&self.store, key.to_string(), value, revision)
            .await
            .map_err(anyhow::Error::from)
    }
}

fn is_cas_conflict(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<kv::CreateError>()
        .is_some_and(|error| error.kind() == kv::CreateErrorKind::AlreadyExists)
        || error
            .downcast_ref::<kv::UpdateError>()
            .is_some_and(|error| error.kind() == kv::UpdateErrorKind::WrongLastRevision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn counter_is_shared_and_clear_resets_it() {
        harnx_core::require_nextest();
        let Some((url, mut child, _store_dir)) = crate::nats_worker::tests::spawn_test_nats().await
        else {
            return;
        };
        let jetstream =
            async_nats::jetstream::new(async_nats::connect(&url).await.expect("connect test NATS"));
        let first = ActivationFailureTracker::ensure(&jetstream, 1)
            .await
            .expect("first tracker");
        let second = ActivationFailureTracker::ensure(&jetstream, 1)
            .await
            .expect("second tracker");

        assert_eq!(first.increment("WORK_NOTIFY_local/42").await.unwrap(), 1);
        assert_eq!(second.increment("WORK_NOTIFY_local/42").await.unwrap(), 2);
        first.clear("WORK_NOTIFY_local/42").await.unwrap();
        assert_eq!(second.increment("WORK_NOTIFY_local/42").await.unwrap(), 1);

        let _ = child.kill();
        let _ = child.wait();
    }
}
