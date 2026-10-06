//! A2A task storage keys and thin get/put/CAS/list helpers.
//!
//! Task and message keys live under `sessions/{storage_key}/a2a/` so that
//! `purge_session_prefix` removes them with the session. The store methods
//! work with raw bytes/JSON values, keeping A2A crate types out of harnx-runtime.

use super::session_prefix;
use anyhow::{bail, Context, Result};
use async_nats::jetstream::kv;
use futures_util::StreamExt;

/// Borrowed full KV key for an A2A record. Existing raw-key callers convert at
/// the boundary; storage operations share the same typed key internally.
#[derive(Clone, Copy)]
pub struct A2aKvKey<'a>(&'a str);

impl<'a> From<&'a str> for A2aKvKey<'a> {
    fn from(value: &'a str) -> Self {
        Self(value)
    }
}

impl<'a> From<&'a String> for A2aKvKey<'a> {
    fn from(value: &'a String) -> Self {
        Self(value.as_str())
    }
}

impl<'a> A2aKvKey<'a> {
    fn as_str(self) -> &'a str {
        self.0
    }
}

/// Key for an A2A task record.
/// Format: sessions/{storage_key}/a2a/tasks/{task_uuid}
pub fn a2a_task_key(storage_key: &str, task_uuid: &str) -> String {
    format!("{}/a2a/tasks/{}", session_prefix(storage_key), task_uuid)
}

/// Key for an A2A message dedupe record.
/// Format: sessions/{storage_key}/a2a/messages/{sha256(messageId)}
pub fn a2a_message_key(storage_key: &str, message_id_hash: &str) -> String {
    format!(
        "{}/a2a/messages/{}",
        session_prefix(storage_key),
        message_id_hash
    )
}

/// Prefix for listing all A2A tasks in a session.
pub fn a2a_tasks_prefix(storage_key: &str) -> String {
    format!("{}/a2a/tasks/", session_prefix(storage_key))
}

/// Prefix for all A2A keys under a session.
pub fn a2a_session_prefix(storage_key: &str) -> String {
    format!("{}/a2a/", session_prefix(storage_key))
}

use crate::nats_session_metadata::SessionMetadataStore;

impl SessionMetadataStore {
    /// Scan live task keys under exactly one session's prefix.
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
            Err(error) if error.kind() == kv::UpdateErrorKind::WrongLastRevision => {
                bail!("A2A {} key already exists: {}", kind.label(), key)
            }
            Err(error) => Err(error)
                .with_context(|| format!("Failed to create A2A {} key '{}'", kind.label(), key)),
        }
    }

    /// CAS update an A2A task record.
    pub async fn update_a2a_task<'a>(
        &self,
        key: impl Into<A2aKvKey<'a>>,
        value: bytes::Bytes,
        revision: u64,
    ) -> Result<u64> {
        let key = key.into().as_str();
        self.kv_store()
            .update(key, value, revision)
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| {
                format!(
                    "Failed to CAS update A2A task key '{}' at revision {}",
                    key, revision
                )
            })
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

#[derive(Clone, Copy)]
enum A2aRecordKind {
    Task,
    Message,
}

impl A2aRecordKind {
    fn label(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Message => "message",
        }
    }
}
