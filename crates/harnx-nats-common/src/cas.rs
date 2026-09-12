//! Resolve ambiguous KV acknowledgements without applying a mutation twice.

use async_nats::jetstream::kv;
use bytes::Bytes;
use std::time::Duration;
use tokio::time::{timeout_at, Instant};

pub async fn update(
    store: &kv::Store,
    key: String,
    value: Bytes,
    revision: u64,
) -> Result<u64, kv::UpdateError> {
    let deadline = Instant::now() + crate::recovery::RECOVERY_TIMEOUT;
    loop {
        let result = timeout_at(deadline, store.update(&key, value.clone(), revision))
            .await
            .map_err(|error| kv::UpdateError::with_source(kv::UpdateErrorKind::TimedOut, error))?;
        match result {
            Ok(revision) => return Ok(revision),
            Err(error)
                if matches!(
                    error.kind(),
                    kv::UpdateErrorKind::TimedOut | kv::UpdateErrorKind::Other
                ) =>
            {
                let entry = timeout_at(deadline, crate::recovery::read(|| store.entry(&key)))
                    .await
                    .map_err(|error| {
                        kv::UpdateError::with_source(kv::UpdateErrorKind::TimedOut, error)
                    })?
                    .map_err(|error| {
                        kv::UpdateError::with_source(kv::UpdateErrorKind::Other, error.to_string())
                    })?;
                let Some(entry) = entry else {
                    return Err(error);
                };
                if confirms_update(&entry, &value, revision) {
                    return Ok(entry.revision);
                }
                if entry.revision != revision {
                    // Another writer advanced the record. It is unknown whether
                    // our update preceded it; do not report a conflict that
                    // invites recomputing and applying the logical mutation.
                    return Err(error);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn confirms_update(entry: &kv::Entry, value: &Bytes, previous_revision: u64) -> bool {
    entry.operation == kv::Operation::Put
        && entry.revision > previous_revision
        && entry.value == *value
}
