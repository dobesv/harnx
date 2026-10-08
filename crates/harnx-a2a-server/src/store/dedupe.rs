//! Process-local and durable message deduplication.
use super::{parse_task_id, A2aStore, ContextAccess, MessageIdentity, StoreError, TaskRecord};
use anyhow::{ensure, Result};
use harnx_core::crypto::sha256;
use harnx_runtime::nats_session_metadata::a2a_message_key;
use lru::LruCache;
use parking_lot::RwLock;
use std::sync::Arc;

/// In-process LRU cache capacity for message dedupe (per export+owner+messageId race window).
const DEDUPE_LRU_CAPACITY: usize = 1024;

/// Create a dedupe LRU cache with default capacity.
pub fn create_dedupe_lru() -> DedupeLru {
    Arc::new(RwLock::new(LruCache::new(
        std::num::NonZero::new(DEDUPE_LRU_CAPACITY).unwrap(),
    )))
}

/// Process-local only: new-context retries are lost on restart and cannot coordinate replicas.
/// LRU cache keyed by (cluster, export public name, owner, message ID) for dedupe races.
pub type DedupeLru = Arc<RwLock<LruCache<DedupeKey, (String, String)>>>;

/// Key for the in-process dedupe LRU.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct DedupeKey {
    pub cluster: String,
    pub export: String,
    pub owner: Option<String>,
    pub message_id: String,
}

/// Task and message fingerprint retained for a new-context retry.
pub struct DedupeEntry {
    pub task_id: String,
    pub fingerprint: String,
}

/// Hash a message ID for dedupe key lookup.
pub fn message_id_hash(message_id: &str) -> String {
    sha256(message_id)
}

/// Compute canonical fingerprint of message parts.
pub fn message_fingerprint(parts: &[a2a_lf::Part]) -> String {
    let mut value = serde_json::to_value(parts).expect("parts serialize");
    value.sort_all_objects();
    let parts_json = serde_json::to_string(&value).expect("parts serialize");
    sha256(&parts_json)
}

impl A2aStore {
    /// Check before busy/terminal rejection. A terminal retry returns its stored snapshot.
    pub async fn dedupe_task(
        &self,
        context: ContextAccess<'_>,
        message: MessageIdentity<'_>,
    ) -> Result<Option<TaskRecord>> {
        let ContextAccess {
            export,
            owner,
            local_id,
        } = context;
        let MessageIdentity {
            message_id,
            fingerprint,
        } = message;
        let key = self
            .resolve_context(export, owner, local_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        let Some((task_id, existing)) = self.get_message_dedupe(&key, message_id).await? else {
            return Ok(None);
        };
        if existing != fingerprint {
            return Err(StoreError::FingerprintMismatch.into());
        }
        ensure!(
            parse_task_id(&task_id)?.0 == local_id,
            "dedupe context mismatch"
        );
        self.get_task(&key, &task_id)
            .await?
            .ok_or(StoreError::NotFound)
            .map(Some)
            .map_err(Into::into)
    }

    /// Check message dedupe cache.
    pub fn check_dedupe_lru(&self, key: &DedupeKey, fingerprint: &str) -> Result<Option<String>> {
        match self.dedupe_lru.write().get(key) {
            Some((task_id, existing)) if existing == fingerprint => Ok(Some(task_id.clone())),
            Some(_) => Err(StoreError::FingerprintMismatch.into()),
            None => Ok(None),
        }
    }

    /// Record a task ID in the dedupe LRU.
    pub fn record_dedupe_lru(&self, key: DedupeKey, entry: DedupeEntry) {
        self.dedupe_lru
            .write()
            .put(key, (entry.task_id, entry.fingerprint));
    }

    /// Write a message dedupe record.
    pub async fn put_message_dedupe(
        &self,
        storage_key: &str,
        message: MessageIdentity<'_>,
        task_id: &str,
    ) -> Result<()> {
        let MessageIdentity {
            message_id,
            fingerprint,
        } = message;
        let hash = message_id_hash(message_id);
        let key = a2a_message_key(storage_key, &hash);
        let record = serde_json::json!({
            "task_id": task_id,
            "fingerprint": fingerprint,
        });
        let payload = serde_json::to_vec(&record).expect("record serializes");

        self.store.put_a2a_message(&key, payload.into()).await?;

        Ok(())
    }

    /// Get existing dedupe record for a message.
    pub async fn get_message_dedupe(
        &self,
        storage_key: &str,
        message_id: &str,
    ) -> Result<Option<(String, String)>> {
        let hash = message_id_hash(message_id);
        let key = a2a_message_key(storage_key, &hash);
        match self.store.get_a2a_message(&key).await? {
            Some((bytes, _)) => {
                let value: serde_json::Value = serde_json::from_slice(&bytes)?;
                let task_id = value["task_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("missing task_id"))?
                    .to_string();
                let fingerprint = value["fingerprint"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("missing fingerprint"))?
                    .to_string();
                Ok(Some((task_id, fingerprint)))
            }
            None => Ok(None),
        }
    }
}
