use async_nats::jetstream::{self, kv};
use futures_util::TryStreamExt;
use harnx_nats_common::{cas, leader_reads, registry};
use serde::{de::DeserializeOwned, Serialize};
use std::{sync::Arc, time::Duration};

use crate::{envelope, Error, Keyring, Result, Snapshot};

#[derive(Clone)]
pub(crate) struct EncryptedBucket {
    pub kv: kv::Store,
    pub keys: Arc<Keyring>,
}

impl std::fmt::Debug for EncryptedBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedBucket")
            .field("bucket", &self.kv.name)
            .finish_non_exhaustive()
    }
}

pub(crate) async fn ensure_bucket(
    js: &jetstream::Context,
    name: &str,
    ttl: Duration,
    replicas: usize,
) -> Result<kv::Store> {
    if replicas == 0 {
        return Err(Error::InvalidRecord);
    }
    // Never silently accept a different TTL. Lease safety depends on all instances agreeing.
    if let Ok(kv) = js
        .create_key_value(kv::Config {
            bucket: name.into(),
            history: 1,
            max_age: ttl,
            num_replicas: replicas,
            ..Default::default()
        })
        .await
    {
        return Ok(kv);
    }
    registry::reconcile_bucket(js, name, replicas, |_| {})
        .await
        .map_err(|_| Error::Storage)?;
    let kv = js.get_key_value(name).await.map_err(|_| Error::Storage)?;
    let config = kv.stream.cached_info().config.clone();
    if config.max_age != ttl || config.max_messages_per_subject != 1 {
        return Err(Error::Storage);
    }
    Ok(kv)
}

pub(crate) async fn entry(kv: &kv::Store, key: &str) -> Result<Option<kv::Entry>> {
    leader_reads::retry_transient(|| leader_reads::entry(kv, key))
        .await
        .map_err(|_| Error::Storage)
}

pub(crate) async fn update(
    kv: &kv::Store,
    key: &str,
    value: Vec<u8>,
    revision: u64,
) -> Result<u64> {
    cas::update(kv, key.to_owned(), value.into(), revision)
        .await
        .map_err(write_error)
}

pub(crate) async fn delete(kv: &kv::Store, key: &str, revision: u64) -> Result<()> {
    // Keep the tombstone: a stale expected-revision=0 create must not resurrect a deletion.
    kv.delete_expect_revision(key, Some(revision))
        .await
        .map_err(write_error)
}

fn write_error(error: kv::UpdateError) -> Error {
    if error.kind() == kv::UpdateErrorKind::WrongLastRevision {
        Error::Conflict
    } else {
        Error::Storage
    }
}

pub(crate) async fn keys(kv: &kv::Store) -> Result<Vec<String>> {
    kv.keys()
        .await
        .map_err(|_| Error::Storage)?
        .try_collect()
        .await
        .map_err(|_| Error::Storage)
}

impl EncryptedBucket {
    pub async fn read<T: DeserializeOwned>(&self, key: &str) -> Result<Snapshot<T>> {
        let Some(entry) = entry(&self.kv, key).await? else {
            return Ok(Snapshot {
                revision: 0,
                record: None,
            });
        };
        let record = if entry.operation == kv::Operation::Put {
            Some(envelope::open(
                &self.keys,
                &self.kv.name,
                key,
                &entry.value,
            )?)
        } else {
            None
        };
        Ok(Snapshot {
            revision: entry.revision,
            record,
        })
    }

    pub async fn write<T: Serialize>(&self, key: &str, value: &T, revision: u64) -> Result<u64> {
        let bytes = envelope::seal(&self.keys, &self.kv.name, key, value)?;
        update(&self.kv, key, bytes, revision).await
    }
}
