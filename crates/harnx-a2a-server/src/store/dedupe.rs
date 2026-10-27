//! Authoritative active identities and immutable message projections; legacy LRU
//! helpers remain available but production admission does not trust them.
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

/// Legacy process-local cache. Shared first-message reservations, not this cache,
/// decide production retries across processes and restarts.
/// LRU cache keyed by (cluster, export public name, owner, message ID) for dedupe races.
pub type DedupeLru = Arc<RwLock<LruCache<DedupeKey, (String, String)>>>;

/// Key for the in-process dedupe LRU.
#[derive(Debug, Clone, Hash, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DedupeKey {
    pub cluster: String,
    pub export: String,
    pub owner: Option<String>,
    pub message_id: String,
}

/// Key for reply dedupe: uses separate storage from initial message dedupe.
/// Reply mappings survive task retirement.
#[allow(dead_code)]
#[derive(Debug, Clone, Hash, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReplyDedupeKey {
    pub cluster: String,
    pub export: String,
    pub owner: Option<String>,
    pub message_id: String,
}

/// Task and message fingerprint retained for dedupe mappings and LRU.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DedupeEntry {
    pub task_id: String,
    pub fingerprint: String,
}

impl DedupeEntry {
    pub fn new(task_id: impl Into<String>, fingerprint: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            fingerprint: fingerprint.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DedupeMappingKind {
    Message,
    Reply,
}

impl DedupeMappingKind {
    fn label(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Reply => "reply",
        }
    }

    fn validate_target(self, target: DedupeMappingTarget<'_>) -> Result<()> {
        // Initial-message mappings historically accept identities already checked
        // by admission; only reply mappings perform these checks at the store API.
        if self == Self::Message {
            return Ok(());
        }
        ensure!(
            !target.message.message_id.is_empty() && !target.message.fingerprint.is_empty(),
            "missing reply mapping identity"
        );
        parse_task_id(target.task_id)?;
        Ok(())
    }

    fn key(self, storage_key: &str, hash: &str) -> String {
        match self {
            Self::Message => a2a_message_key(storage_key, hash),
            Self::Reply => reply_dedupe_key(storage_key, hash),
        }
    }
}

/// Target parameters for writing a dedupe mapping.
#[derive(Clone, Copy, Debug)]
pub struct DedupeMappingTarget<'a> {
    pub message: MessageIdentity<'a>,
    pub task_id: &'a str,
}

impl<'a> DedupeMappingTarget<'a> {
    pub fn new(message: MessageIdentity<'a>, task_id: &'a str) -> Self {
        Self { message, task_id }
    }
}

fn decode_dedupe_entry(bytes: &[u8]) -> Result<DedupeEntry> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let task_id = value["task_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing task_id"))?
        .to_string();
    let fingerprint = value["fingerprint"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing fingerprint"))?
        .to_string();
    Ok(DedupeEntry {
        task_id,
        fingerprint,
    })
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
    async fn check_active_task_dedupe(
        &self,
        key: &str,
        message: MessageIdentity<'_>,
    ) -> Result<Option<TaskRecord>> {
        let MessageIdentity {
            message_id,
            fingerprint,
        } = message;
        if let Some(active) = self
            .read_context(key)
            .await?
            .and_then(|c| c.document.state.active)
            .filter(|active| active.message.message_id == message_id)
        {
            ensure!(
                active.message.fingerprint == fingerprint,
                StoreError::FingerprintMismatch
            );
            return Ok(Some(active.snapshot));
        }
        Ok(None)
    }

    async fn lookup_recorded_dedupe(
        &self,
        key: &str,
        local_id: &str,
        message: MessageIdentity<'_>,
    ) -> Result<Option<TaskRecord>> {
        let Some(existing) = self.get_message_dedupe(key, message.message_id).await? else {
            return Ok(None);
        };
        if existing.fingerprint != message.fingerprint {
            return Err(StoreError::FingerprintMismatch.into());
        }
        ensure!(
            parse_task_id(&existing.task_id)?.0 == local_id,
            "dedupe context mismatch"
        );
        self.get_task(key, &existing.task_id)
            .await?
            .ok_or(StoreError::NotFound)
            .map(Some)
            .map_err(Into::into)
    }

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
        let key = self
            .resolve_context(export, owner, local_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        if let Some(record) = self.check_active_task_dedupe(&key, message).await? {
            return Ok(Some(record));
        }
        self.lookup_recorded_dedupe(&key, local_id, message).await
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

    async fn read_dedupe_mapping(
        &self,
        kind: DedupeMappingKind,
        storage_key: &str,
        message_id: &str,
    ) -> Result<Option<DedupeEntry>> {
        match kind {
            DedupeMappingKind::Message => self.get_message_dedupe(storage_key, message_id).await,
            DedupeMappingKind::Reply => self.get_reply_dedupe(storage_key, message_id).await,
        }
    }

    /// Persist an initial-message or reply mapping with create-only CAS.
    pub async fn put_dedupe_mapping(
        &self,
        kind: DedupeMappingKind,
        storage_key: &str,
        target: DedupeMappingTarget<'_>,
    ) -> Result<()> {
        let MessageIdentity {
            message_id,
            fingerprint,
        } = target.message;
        kind.validate_target(target)?;
        let hash = message_id_hash(message_id);
        let key = kind.key(storage_key, &hash);
        let record = DedupeEntry::new(target.task_id, fingerprint);
        let payload = serde_json::to_vec(&record).expect("record serializes");

        if self
            .store
            .kv_store()
            .update(&key, payload.into(), 0)
            .await
            .is_err()
        {
            let saved = self
                .read_dedupe_mapping(kind, storage_key, message_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("{} mapping unconfirmed", kind.label()))?;
            ensure!(
                saved.fingerprint == fingerprint,
                StoreError::FingerprintMismatch
            );
            ensure!(
                saved.task_id == target.task_id,
                format!("{} mapping identity conflict", kind.label())
            );
        }
        Ok(())
    }

    /// Get existing dedupe record for a message.
    pub async fn get_message_dedupe(
        &self,
        storage_key: &str,
        message_id: &str,
    ) -> Result<Option<DedupeEntry>> {
        let hash = message_id_hash(message_id);
        let key = a2a_message_key(storage_key, &hash);
        match self.store.get_a2a_message(&key).await? {
            Some((bytes, _)) => decode_dedupe_entry(&bytes).map(Some),
            None => Ok(None),
        }
    }

    // ========================================================================
    // Reply dedupe (Task 5)
    // ========================================================================

    /// Check existing reply dedupe mapping. Returns DedupeEntry if found.
    pub async fn get_reply_dedupe(
        &self,
        storage_key: &str,
        message_id: &str,
    ) -> Result<Option<DedupeEntry>> {
        let hash = message_id_hash(message_id);
        let key = reply_dedupe_key(storage_key, &hash);
        match harnx_nats_common::leader_reads::entry(self.store.kv_store(), &key).await? {
            Some(entry) if entry.operation == async_nats::jetstream::kv::Operation::Put => {
                decode_dedupe_entry(&entry.value).map(Some)
            }
            _ => Ok(None),
        }
    }
}

fn reply_dedupe_key(storage_key: &str, message_id_hash: &str) -> String {
    format!("sessions/{storage_key}/a2a/reply/{message_id_hash}")
}
