//! SessionMetadataStore A2A methods.
//!
//! A2A task records are stored as opaque bytes in SessionMetadataStore to avoid
//! depending on a2a_lf in harnx-runtime. All operations that write/read tasks
//! work with raw bytes/JSON values, keeping A2A crate types out of harnx-runtime.

use super::a2a_index::{a2a_task_index_key, TaskIndex};
use super::session_prefix;
use crate::nats_session_metadata::SessionMetadataStore;
use anyhow::{Context, Result};
use async_nats::jetstream::kv;
use futures_util::StreamExt;

/// Borrowed full KV key for an A2A record. Existing raw-key callers convert at
/// the store boundary; internally we borrow the underlying string.
#[derive(Clone, Copy)]
pub struct A2aKvKey<'a>(&'a str);

impl<'a> A2aKvKey<'a> {
    pub fn as_str(self) -> &'a str {
        self.0
    }
}

impl<'a> From<&'a str> for A2aKvKey<'a> {
    fn from(s: &'a str) -> Self {
        Self(s)
    }
}

impl<'a> From<&'a String> for A2aKvKey<'a> {
    fn from(s: &'a String) -> Self {
        Self(s.as_str())
    }
}

/// Key format: sessions/{storage_key}/a2a/tasks/{uuid}
pub fn a2a_task_key(storage_key: &str, task_uuid: &str) -> String {
    format!("{}/a2a/tasks/{}", session_prefix(storage_key), task_uuid)
}

/// Prefix for all tasks in a session: sessions/{storage_key}/a2a/tasks/
pub fn a2a_tasks_prefix(storage_key: &str) -> String {
    format!("{}/a2a/tasks/", session_prefix(storage_key))
}

/// Key format: sessions/{storage_key}/a2a/messages/{sha256}
pub fn a2a_message_key(storage_key: &str, sha256_hex: &str) -> String {
    format!(
        "{}/a2a/messages/{}",
        session_prefix(storage_key),
        sha256_hex
    )
}

/// Prefix for all A2A keys in a session: sessions/{storage_key}/a2a/
pub fn a2a_session_prefix(storage_key: &str) -> String {
    format!("{}/a2a/", session_prefix(storage_key))
}

impl SessionMetadataStore {
    /// Scan live task keys under exactly one session's prefix.
    /// DEPRECATED: This performs a full-bucket scan. Use `get_a2a_task_index`
    /// and `get_a2a_task` for efficient pagination.
    pub async fn list_a2a_tasks(&self, storage_key: &str) -> Result<Vec<(Vec<u8>, u64)>> {
        let prefix = a2a_tasks_prefix(storage_key);
        let mut keys = self.kv_store().keys().await?;
        let mut records = Vec::new();
        while let Some(key) = keys.next().await {
            let key = key?;
            if key.starts_with(&prefix) {
                if let Some(record) = self.get_a2a_task(&key).await? {
                    records.push(record);
                }
            }
        }
        Ok(records)
    }

    /// Get the per-session task index.
    /// Returns Ok(None) if missing (key does not exist in KV); Ok(Some((index, revision))) if present.
    pub async fn get_a2a_task_index(&self, storage_key: &str) -> Result<Option<(TaskIndex, u64)>> {
        let key = a2a_task_index_key(storage_key);
        match self.get_a2a_record((&key).into()).await? {
            Some((bytes, revision)) => {
                let mut index: TaskIndex =
                    serde_json::from_slice(&bytes).context("Failed to deserialize task index")?;
                index.sort();
                Ok(Some((index, revision)))
            }
            None => Ok(None),
        }
    }

    /// Put the per-session task index with CAS.
    /// On creation (revision None), uses create-only semantics (`update(key, value, 0)`).
    pub async fn put_a2a_task_index(
        &self,
        storage_key: &str,
        index: &TaskIndex,
        revision: Option<u64>,
    ) -> Result<u64> {
        let key = a2a_task_index_key(storage_key);
        let value = serde_json::to_vec(index).context("Failed to serialize task index")?;
        let status = self.kv_store().status().await?;
        let stream_max = status.info.config.max_message_size;
        let server_max = self.client.server_info().max_payload;
        let max_size = match (stream_max > 0, server_max > 0) {
            (true, true) => (stream_max as usize).min(server_max),
            (true, false) => stream_max as usize,
            (false, true) => server_max,
            (false, false) => 1024 * 1024,
        };
        if value.len() > max_size {
            anyhow::bail!(
                "task index exceeds maximum size limit (serialized size {} bytes > {} max)",
                value.len(),
                max_size
            );
        }
        match revision {
            Some(rev) => {
                self.update_a2a_record(A2aRecordUpdate {
                    key: (&key).into(),
                    value: value.into(),
                    revision: rev,
                    kind: A2aRecordKind::Index,
                })
                .await
            }
            None => {
                self.create_a2a_record((&key).into(), value.into(), A2aRecordKind::Index)
                    .await
            }
        }
    }

    /// Get an A2A task record by its full key.
    pub async fn get_a2a_task<'a>(
        &self,
        key: impl Into<A2aKvKey<'a>>,
    ) -> Result<Option<(Vec<u8>, u64)>> {
        self.get_a2a_record(key.into()).await
    }

    async fn get_a2a_record(&self, key: A2aKvKey<'_>) -> Result<Option<(Vec<u8>, u64)>> {
        let entry = self.leader_entry(key.as_str()).await?;
        match entry {
            Some(entry) if matches!(entry.operation, kv::Operation::Put) => {
                Ok(Some((entry.value.to_vec(), entry.revision)))
            }
            Some(_) | None => Ok(None),
        }
    }

    /// Put an A2A task record (create-only).
    pub async fn put_a2a_task<'a>(
        &self,
        key: impl Into<A2aKvKey<'a>>,
        value: bytes::Bytes,
    ) -> Result<u64> {
        self.create_a2a_record(key.into(), value, A2aRecordKind::Task)
            .await
    }

    async fn create_a2a_record(
        &self,
        key: A2aKvKey<'_>,
        value: bytes::Bytes,
        kind: A2aRecordKind,
    ) -> Result<u64> {
        let key = key.as_str();
        match self.kv_store().update(key, value, 0).await {
            Ok(revision) => Ok(revision),
            Err(error) => {
                let context = format!("Failed to create A2A {} key '{}'", kind.label(), key);
                Err(anyhow::Error::from(error).context(context))
            }
        }
    }

    async fn update_a2a_record(&self, update: A2aRecordUpdate<'_>) -> Result<u64> {
        let A2aRecordUpdate {
            key,
            value,
            revision,
            kind,
        } = update;
        let key = key.as_str();
        self.kv_store()
            .update(key, value, revision)
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| {
                format!(
                    "Failed to CAS update A2A {} key '{}' at revision {}",
                    kind.label(),
                    key,
                    revision
                )
            })
    }

    /// CAS update an A2A task record.
    pub async fn update_a2a_task<'a>(
        &self,
        key: impl Into<A2aKvKey<'a>>,
        value: bytes::Bytes,
        revision: u64,
    ) -> Result<u64> {
        self.update_a2a_record(A2aRecordUpdate {
            key: key.into(),
            value,
            revision,
            kind: A2aRecordKind::Task,
        })
        .await
    }

    /// Put an A2A message dedupe record (create-only).
    pub async fn put_a2a_message<'a>(
        &self,
        key: impl Into<A2aKvKey<'a>>,
        value: bytes::Bytes,
    ) -> Result<u64> {
        self.create_a2a_record(key.into(), value, A2aRecordKind::Message)
            .await
    }

    /// Get an A2A message dedupe record.
    pub async fn get_a2a_message<'a>(
        &self,
        key: impl Into<A2aKvKey<'a>>,
    ) -> Result<Option<(Vec<u8>, u64)>> {
        self.get_a2a_record(key.into()).await
    }
}

struct A2aRecordUpdate<'a> {
    key: A2aKvKey<'a>,
    value: bytes::Bytes,
    revision: u64,
    kind: A2aRecordKind,
}

#[derive(Clone, Copy)]
enum A2aRecordKind {
    Task,
    Message,
    Index,
}

impl A2aRecordKind {
    fn label(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Message => "message",
            Self::Index => "index",
        }
    }
}
