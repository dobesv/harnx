//! Session-scoped A2A task store.
//!
//! Persists A2A state in the harnx session's KV namespace so the existing GC removes it.
//! Keys use the storage key (SHA-256 of agent/sid), not the local session id shown to clients.
//!
//! Key layout:
//! - `sessions/{storage_key}/meta` - session metadata (includes `dev.harnx.a2a` binding)
//! - `sessions/{storage_key}/a2a/tasks/{uuid}` - task records
//! - `sessions/{storage_key}/a2a/messages/{sha256}` - message dedupe records
//!
//! Task ID format: `{local_id}.{uuid}` - local_id has no `.` per base64url alphabet.

use std::sync::Arc;

use a2a_lf::{Artifact, Message, Task, TaskStatus};
use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Utc};
use lru::LruCache;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use harnx_core::{access_rules::AccessRules, crypto::sha256};
use harnx_runtime::nats_session_metadata::{a2a_message_key, a2a_task_key, SessionMetadataStore};

use crate::exports::Export;
use crate::identity::Principal;

/// Extension namespace for the A2A session binding in metadata.extensions.
pub const A2A_BINDING_NAMESPACE: &str = "dev.harnx.a2a";

/// Version of the A2A binding schema.
pub const A2A_BINDING_VERSION: u32 = 1;

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

/// Session binding stored in `dev.harnx.a2a` extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2aBinding {
    pub version: u32,
    pub export: String,
    pub agent: String,
    pub cluster: String,
    pub owner: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Internal task record persisted to KV.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub version: u32,
    /// A2A Task snapshot (id, context_id, status, artifacts, history).
    pub task: Task,
    /// Internal user message ID from admission.
    pub user_msg_id: String,
    /// Internal user message sequence from admission.
    pub user_msg_seq: u64,
    /// Execution ID for turn correlation.
    pub execution_id: String,
    /// Task-local revision, incremented on each update (not the bucket-wide KV revision).
    pub revision: u64,
    /// When the task was created.
    pub created_at: DateTime<Utc>,
    /// When the task was last updated.
    pub updated_at: DateTime<Utc>,
}

/// Parse a task ID into its local session ID and UUID components.
/// Format: `{local_id}.{uuid}`, with a lowercase hyphenated UUID.
pub fn parse_task_id(task_id: &str) -> Result<(&str, &str)> {
    let dot_pos = task_id
        .find('.')
        .ok_or_else(|| anyhow::anyhow!("task ID must contain '.'"))?;
    let local_id = &task_id[..dot_pos];
    let uuid = &task_id[dot_pos + 1..];
    ensure!(!local_id.is_empty(), "local ID must not be empty");
    let parsed = uuid::Uuid::parse_str(uuid).context("invalid task UUID")?;
    // UUID text is also the KV suffix; aliases must not select different keys.
    ensure!(
        parsed.hyphenated().to_string() == uuid,
        "task UUID must be lowercase and hyphenated"
    );
    ensure!(
        local_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid local session ID"
    );
    Ok((local_id, uuid))
}

/// Format a task ID from local session ID and UUID.
pub fn format_task_id(local_id: &str, uuid: &str) -> String {
    format!("{}.{}", local_id, uuid)
}

/// Assert local_id has no `.` (valid per base64url alphabet).
pub fn assert_local_id_no_dot(local_id: &str) -> Result<()> {
    ensure!(
        !local_id.contains('.'),
        "local session ID must not contain '.' (got '{}')",
        local_id
    );
    Ok(())
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

/// Check the session binding against the resolved export and owner.
/// Mismatched exports and unauthorized owners look like missing contexts.
pub fn validate_binding(
    binding: &A2aBinding,
    export: &Export,
    owner: &Principal,
    access_rules: Option<&AccessRules>,
) -> bool {
    binding_matches_export(binding, export)
        && match access_rules {
            Some(rules) => rules.can_access_session(
                &export.agent_ref(),
                &owner.user_id().into_iter().collect::<Vec<_>>(),
                binding.owner.as_deref(),
            ),
            None => binding.owner.as_deref() == owner.user_id(),
        }
}

fn binding_matches_export(binding: &A2aBinding, export: &Export) -> bool {
    let expected = (
        A2A_BINDING_VERSION,
        export.public_name.as_str(),
        export.agent.as_str(),
        export.cluster.as_deref().unwrap_or("__local__"),
    );
    let actual = (
        binding.version,
        binding.export.as_str(),
        binding.agent.as_str(),
        binding.cluster.as_str(),
    );
    actual == expected
}

/// Create a new task ID with a fresh UUID.
pub fn new_task_id(local_id: &str) -> String {
    assert_local_id_no_dot(local_id).expect("local_id validated at session creation");
    let uuid = uuid::Uuid::new_v4();
    format_task_id(local_id, &uuid.hyphenated().to_string())
}

/// Errors callers can map without matching broker error strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    NotFound,
    FingerprintMismatch,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotFound => "task not found",
            Self::FingerprintMismatch => "messageId reused with different parts",
        })
    }
}
impl std::error::Error for StoreError {}

/// Authorization scope for a context, derived from the resolved export.
#[derive(Clone, Copy)]
pub struct ContextAccess<'a> {
    pub export: &'a Export,
    pub owner: &'a Principal,
    pub local_id: &'a str,
}

#[derive(Clone, Copy)]
pub struct TaskAccess<'a> {
    pub export: &'a Export,
    pub owner: &'a Principal,
    pub task_id: &'a str,
}

#[derive(Clone, Copy)]
pub struct MessageIdentity<'a> {
    pub message_id: &'a str,
    pub fingerprint: &'a str,
}

/// Runtime correlation fields are empty until external admission completes.
pub struct TaskSeed {
    pub task: Task,
    pub user_msg_id: String,
    pub user_msg_seq: u64,
    pub execution_id: String,
}

#[derive(Clone, Copy)]
pub struct TaskVersion<'a> {
    pub storage_key: &'a str,
    pub task_id: &'a str,
    pub revision: u64,
}

#[derive(Default)]
pub struct TaskChanges {
    pub status: Option<TaskStatus>,
    pub artifacts: Option<Vec<Artifact>>,
    pub history: Option<Vec<Message>>,
}

/// A2A task store operations.
pub struct A2aStore {
    store: SessionMetadataStore,
    dedupe_lru: DedupeLru,
    access_rules: Option<Arc<AccessRules>>,
}

impl A2aStore {
    /// Create a new A2A store wrapper.
    pub fn new(store: SessionMetadataStore) -> Self {
        Self::new_with_access_rules(store, None)
    }

    /// Use the same rules for every context lookup, including runner operations.
    pub fn new_with_access_rules(
        store: SessionMetadataStore,
        access_rules: Option<Arc<AccessRules>>,
    ) -> Self {
        Self {
            store,
            dedupe_lru: create_dedupe_lru(),
            access_rules,
        }
    }

    pub fn access_rules(&self) -> Option<&AccessRules> {
        self.access_rules.as_deref()
    }

    /// Resolve and authorize before resuming a runtime session. Never creates metadata.
    pub async fn resolve_context(
        &self,
        export: &Export,
        owner: &Principal,
        local_id: &str,
    ) -> Result<Option<String>> {
        let key = harnx_core::session_identity::session_key(Some(&export.agent), local_id);
        Ok(self
            .get_binding(&key)
            .await?
            .filter(|binding| validate_binding(binding, export, owner, self.access_rules()))
            .map(|_| key))
    }

    /// Client-facing lookup derives the storage key only from the resolved export.
    pub async fn get_task_for_export(
        &self,
        export: &Export,
        owner: &Principal,
        task_id: &str,
    ) -> Result<Option<TaskRecord>> {
        let Ok((local_id, _)) = parse_task_id(task_id) else {
            return Ok(None);
        };
        let Some(key) = self.resolve_context(export, owner, local_id).await? else {
            return Ok(None);
        };
        self.get_task(&key, task_id).await
    }

    /// List only this authorized context's task prefix. None means not-found.
    pub async fn list_tasks(
        &self,
        export: &Export,
        owner: &Principal,
        local_id: &str,
    ) -> Result<Option<Vec<TaskRecord>>> {
        let Some(key) = self.resolve_context(export, owner, local_id).await? else {
            return Ok(None);
        };
        let mut records = Vec::new();
        for (bytes, _) in self.store.list_a2a_tasks(&key).await? {
            let record: TaskRecord = serde_json::from_slice(&bytes)?;
            ensure!(
                record.version == 1 && record.task.context_id == local_id,
                "invalid task record"
            );
            records.push(record);
        }
        records.sort_by(|a, b| a.task.id.cmp(&b.task.id));
        Ok(Some(records))
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

    /// Get the session binding from metadata extensions.
    pub async fn get_binding(&self, storage_key: &str) -> Result<Option<A2aBinding>> {
        let Some(record) = self.store.get(storage_key).await? else {
            return Ok(None);
        };
        record
            .metadata
            .extensions
            .get(A2A_BINDING_NAMESPACE)
            .map(|value| {
                serde_json::from_value(value.clone())
                    .with_context(|| format!("Invalid A2A binding in {}", storage_key))
            })
            .transpose()
    }

    /// Write the A2A binding to a new session's extensions.
    pub async fn bind_context(
        &self,
        storage_key: &str,
        export: &Export,
        owner: &Principal,
    ) -> Result<()> {
        let mut record = self
            .store
            .get(storage_key)
            .await?
            .ok_or(StoreError::NotFound)?;
        let binding = A2aBinding {
            version: A2A_BINDING_VERSION,
            export: export.public_name.clone(),
            agent: export.agent.clone(),
            cluster: export.cluster.as_deref().unwrap_or("__local__").into(),
            owner: owner.user_id().map(str::to_owned),
            created_at: Utc::now(),
        };
        self.write_binding(storage_key, &mut record, &binding).await
    }

    /// Low-level write-once binding patch; only call during session creation.
    pub async fn write_binding(
        &self,
        storage_key: &str,
        record: &mut harnx_runtime::nats_session_metadata::MetadataRecord,
        binding: &A2aBinding,
    ) -> Result<()> {
        ensure!(
            storage_key == record.metadata.storage_key(),
            "binding storage key mismatch"
        );
        ensure!(
            binding.agent == record.metadata.agent.name().unwrap_or_default(),
            "binding agent mismatch"
        );
        ensure!(
            binding.version == A2A_BINDING_VERSION,
            "unsupported binding version"
        );
        assert_local_id_no_dot(&record.metadata.session_id)?;
        let binding_json = serde_json::to_value(binding)?;
        // The runtime creates session metadata before the frontend binds it.
        // Check absence inside the CAS closure so a stale caller cannot rebind it.
        *record = self
            .store
            .patch(storage_key, |metadata| {
                ensure!(
                    !metadata.extensions.contains_key(A2A_BINDING_NAMESPACE),
                    "A2A binding already exists"
                );
                metadata
                    .extensions
                    .insert(A2A_BINDING_NAMESPACE.into(), binding_json.clone());
                Ok(())
            })
            .await?;

        Ok(())
    }

    /// Trusted runner lookup. RPC callers must use `get_task_for_export` for ownership checks.
    pub async fn get_task(&self, storage_key: &str, task_id: &str) -> Result<Option<TaskRecord>> {
        let (_, uuid) = parse_task_id(task_id)?;
        let key = a2a_task_key(storage_key, uuid);
        match self.store.get_a2a_task(&key).await? {
            Some((bytes, _revision)) => {
                let record: TaskRecord = serde_json::from_slice(&bytes)
                    .with_context(|| format!("Failed to deserialize task {}", task_id))?;
                ensure!(
                    record.task.id == task_id,
                    "Task ID mismatch: expected {}, found {}",
                    task_id,
                    record.task.id
                );
                ensure!(record.version == 1, "unsupported task record version");
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }

    /// Create a new task record.
    pub async fn create_task(&self, storage_key: &str, seed: TaskSeed) -> Result<TaskRecord> {
        let TaskSeed {
            task,
            user_msg_id,
            user_msg_seq,
            execution_id,
        } = seed;
        let (_, uuid) = parse_task_id(&task.id)?;
        let key = a2a_task_key(storage_key, uuid);
        let now = Utc::now();
        ensure!(
            task.context_id == parse_task_id(&task.id)?.0,
            "task context mismatch"
        );
        let record = TaskRecord {
            version: 1,
            task,
            user_msg_id,
            user_msg_seq,
            execution_id,
            revision: 1,
            created_at: now,
            updated_at: now,
        };
        let payload = serde_json::to_vec(&record)?;
        let _revision = self.store.put_a2a_task(&key, payload.into()).await?;
        Ok(record)
    }

    /// Update a task record with CAS.
    /// Returns Ok(record) on success, Err("conflict") on CAS mismatch.
    pub async fn update_task(
        &self,
        version: TaskVersion<'_>,
        changes: TaskChanges,
    ) -> Result<TaskRecord> {
        let TaskChanges {
            status,
            artifacts,
            history,
        } = changes;
        self.mutate_task(version, move |record| {
            if let Some(s) = status {
                record.task.status = s;
            }
            if let Some(a) = artifacts {
                record.task.artifacts = Some(a);
            }
            if let Some(h) = history {
                record.task.history = Some(h);
            }
        })
        .await
    }

    /// Persist turn correlation immediately after external admission.
    pub async fn update_admission(
        &self,
        version: TaskVersion<'_>,
        appended: &harnx_runtime::nats_session::AppendedPrompt,
    ) -> Result<TaskRecord> {
        self.mutate_task(version, |record| {
            record.user_msg_id = appended.user_msg_id().to_owned();
            record.user_msg_seq = appended.user_msg_seq();
            record.execution_id = appended.execution_id().unwrap_or_default().to_owned();
        })
        .await
    }

    async fn mutate_task(
        &self,
        version: TaskVersion<'_>,
        mutate: impl FnOnce(&mut TaskRecord),
    ) -> Result<TaskRecord> {
        let TaskVersion {
            storage_key,
            task_id,
            revision: expected_revision,
        } = version;
        let (_, uuid) = parse_task_id(task_id)?;
        let key = a2a_task_key(storage_key, uuid);

        // Load existing
        let (bytes, current_revision) = self
            .store
            .get_a2a_task(&key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Task not found: {}", task_id))?;

        let mut record: TaskRecord = serde_json::from_slice(&bytes)?;
        ensure!(record.version == 1, "unsupported task record version");
        ensure!(record.task.id == task_id, "task ID mismatch");
        if record.revision != expected_revision {
            bail!(
                "CAS conflict: expected revision {}, found {}",
                expected_revision,
                record.revision
            );
        }

        mutate(&mut record);
        record.revision += 1;
        record.updated_at = Utc::now();

        // The JSON revision is task-local; NATS revisions span the entire bucket.
        let payload = serde_json::to_vec(&record)?;
        self.store
            .update_a2a_task(&key, payload.into(), current_revision)
            .await?;

        Ok(record)
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
    pub fn record_dedupe_lru(&self, key: DedupeKey, task_id: String, fingerprint: String) {
        self.dedupe_lru.write().put(key, (task_id, fingerprint));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_format_and_parse() {
        harnx_core::require_nextest();
        let local_id = "abc123"; // base64url, no '.'
        let uuid = "01234567-89ab-cdef-0123-456789abcdef";
        let task_id = format_task_id(local_id, uuid);
        assert_eq!(task_id, "abc123.01234567-89ab-cdef-0123-456789abcdef");

        let (parsed_local, parsed_uuid) = parse_task_id(&task_id).unwrap();
        assert_eq!(parsed_local, local_id);
        assert_eq!(parsed_uuid, uuid);
    }

    enum NoncanonicalUuid {
        Uppercase,
        Simple,
        Braced,
        Urn,
    }

    fn assert_noncanonical_uuid_rejected(variant: NoncanonicalUuid) {
        harnx_core::require_nextest();
        let uuid = match variant {
            NoncanonicalUuid::Uppercase => "01234567-89AB-CDEF-0123-456789ABCDEF",
            NoncanonicalUuid::Simple => "0123456789abcdef0123456789abcdef",
            NoncanonicalUuid::Braced => "{01234567-89ab-cdef-0123-456789abcdef}",
            NoncanonicalUuid::Urn => "urn:uuid:01234567-89ab-cdef-0123-456789abcdef",
        };
        // These are valid UUID spellings, but aren't valid storage key suffixes.
        assert!(uuid::Uuid::parse_str(uuid).is_ok());
        let error = parse_task_id(&format_task_id("abc123", uuid)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "task UUID must be lowercase and hyphenated"
        );
    }

    #[test]
    fn task_id_rejects_uppercase_uuid() {
        assert_noncanonical_uuid_rejected(NoncanonicalUuid::Uppercase);
    }

    #[test]
    fn task_id_rejects_simple_uuid() {
        assert_noncanonical_uuid_rejected(NoncanonicalUuid::Simple);
    }

    #[test]
    fn task_id_rejects_braced_uuid() {
        assert_noncanonical_uuid_rejected(NoncanonicalUuid::Braced);
    }

    #[test]
    fn task_id_rejects_urn_uuid() {
        assert_noncanonical_uuid_rejected(NoncanonicalUuid::Urn);
    }

    #[test]
    fn task_id_rejects_dot_in_local_id() {
        harnx_core::require_nextest();
        assert!(parse_task_id("abc.123.uuid").is_err());
        let result = assert_local_id_no_dot("abc.123");
        assert!(result.is_err());
    }

    #[test]
    fn assert_local_id_no_dot_passes() {
        harnx_core::require_nextest();
        assert_local_id_no_dot("abc123").unwrap();
        assert_local_id_no_dot("azAZ09-_").unwrap();
    }

    #[test]
    fn assert_local_id_no_dot_fails() {
        harnx_core::require_nextest();
        assert!(assert_local_id_no_dot("abc.123").is_err());
    }

    #[test]
    fn new_task_id_format() {
        harnx_core::require_nextest();
        let local_id = "testSid";
        let task_id = new_task_id(local_id);
        assert!(task_id.starts_with("testSid."));
        assert!(uuid::Uuid::parse_str(&task_id[8..]).is_ok());
    }

    #[test]
    fn message_fingerprint_is_deterministic() {
        harnx_core::require_nextest();
        let parts: Vec<a2a_lf::Part> = vec![a2a_lf::Part::text("hello")];
        let fp1 = message_fingerprint(&parts);
        let fp2 = message_fingerprint(&parts);
        assert_eq!(fp1, fp2);
        assert_eq!(fp1.len(), 64);
    }

    #[test]
    fn message_fingerprint_differs_on_change() {
        harnx_core::require_nextest();
        let parts1: Vec<a2a_lf::Part> = vec![a2a_lf::Part::text("hello")];
        let parts2: Vec<a2a_lf::Part> = vec![a2a_lf::Part::text("world")];
        let fp1 = message_fingerprint(&parts1);
        let fp2 = message_fingerprint(&parts2);
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn message_fingerprint_sorts_nested_objects_but_preserves_part_order() {
        harnx_core::require_nextest();
        let a: a2a_lf::Part =
            serde_json::from_str(r#"{"data":{"z":1,"a":{"y":2,"b":3}}}"#).unwrap();
        let b: a2a_lf::Part =
            serde_json::from_str(r#"{"data":{"a":{"b":3,"y":2},"z":1}}"#).unwrap();
        assert_eq!(
            message_fingerprint(std::slice::from_ref(&a)),
            message_fingerprint(&[b])
        );
        assert_ne!(
            message_fingerprint(&[a.clone(), a2a_lf::Part::text("text")]),
            message_fingerprint(&[a2a_lf::Part::text("text"), a])
        );
    }

    fn binding_fixture() -> (A2aBinding, Export) {
        let binding = A2aBinding {
            version: A2A_BINDING_VERSION,
            export: "public-agent".into(),
            agent: "pkg/agent".into(),
            cluster: "local".into(),
            owner: Some("alice".into()),
            created_at: Utc::now(),
        };
        let export = Export {
            public_name: binding.export.clone(),
            agent: binding.agent.clone(),
            cluster: Some(binding.cluster.clone()),
            card_meta: crate::exports::AgentCardMeta {
                name: "Test".into(),
                description: "Test agent".into(),
                version: "1.0".into(),
                conversation_starters: vec![],
            },
            lookup_keys: vec![binding.export.clone()],
        };
        (binding, export)
    }

    struct BindingCase {
        public_name: &'static str,
        owner: &'static str,
        matches: bool,
    }

    const BINDING_CASES: [BindingCase; 3] = [
        BindingCase {
            public_name: "public-agent",
            owner: "alice",
            matches: true,
        },
        BindingCase {
            public_name: "other-agent",
            owner: "alice",
            matches: false,
        },
        BindingCase {
            public_name: "public-agent",
            owner: "bob",
            matches: false,
        },
    ];

    fn assert_binding_case(case: &BindingCase) {
        harnx_core::require_nextest();
        let (binding, mut export) = binding_fixture();
        export.public_name = case.public_name.into();
        assert_eq!(
            validate_binding(&binding, &export, &Principal::User(case.owner.into()), None),
            case.matches
        );
    }

    #[test]
    fn access_validate_binding_scopes_preserve_export_version_agent_cluster_checks() {
        harnx_core::require_nextest();
        let rules = AccessRules::from_yaml(
            "rules:\n  - agents: [pkg/agent@local]\n    users: [alice, bob]\n  - agents: [pkg/agent@local]\n    users: [admin]\n    scopes: [admin]\n",
        ).unwrap();
        let (binding, export) = binding_fixture();
        let alice = Principal::User("alice".into());
        let bob = Principal::User("bob".into());
        let admin = Principal::User("admin".into());
        assert!(validate_binding(&binding, &export, &alice, Some(&rules)));
        assert!(!validate_binding(&binding, &export, &bob, Some(&rules)));
        assert!(validate_binding(&binding, &export, &admin, Some(&rules)));
        assert!(!validate_binding(&binding, &export, &admin, None));
        let mut legacy = binding.clone();
        legacy.owner = None;
        assert!(!validate_binding(&legacy, &export, &alice, Some(&rules)));
        assert!(validate_binding(&legacy, &export, &admin, Some(&rules)));
        assert!(validate_binding(
            &legacy,
            &export,
            &Principal::Anonymous,
            None
        ));
        assert!(!validate_binding(
            &legacy,
            &export,
            &Principal::Anonymous,
            Some(&rules)
        ));
        for field in ["version", "export", "agent", "cluster"] {
            let mut wrong = binding.clone();
            match field {
                "version" => wrong.version += 1,
                "export" => wrong.export = "other-export".into(),
                "agent" => wrong.agent = "other-agent".into(),
                "cluster" => wrong.cluster = "other-cluster".into(),
                _ => unreachable!(),
            }
            assert!(
                !validate_binding(&wrong, &export, &admin, Some(&rules)),
                "{field}"
            );
        }
    }

    #[test]
    fn validate_binding_matches() {
        assert_binding_case(&BINDING_CASES[0]);
    }

    #[test]
    fn validate_binding_wrong_export() {
        assert_binding_case(&BINDING_CASES[1]);
    }

    #[test]
    fn validate_binding_wrong_owner() {
        assert_binding_case(&BINDING_CASES[2]);
    }
}
