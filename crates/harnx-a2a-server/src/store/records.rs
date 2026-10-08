//! Task record lookup, creation, CAS updates and exact-key notifications.
use super::index::IndexWrite;
use super::{parse_task_id, A2aStore};
use crate::{exports::Export, identity::Principal};
use a2a_lf::{Artifact, Message, Task, TaskStatus};
use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Utc};
use harnx_runtime::nats_session_metadata::a2a_task_key;
use serde::{Deserialize, Serialize};

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
    /// In-process stream cursor. Never used for durable CAS or serialized to KV.
    #[serde(skip)]
    pub stream_seq: u64,
    /// When the task was created.
    pub created_at: DateTime<Utc>,
    /// When the task was last updated.
    pub updated_at: DateTime<Utc>,
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

impl A2aStore {
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

    /// Establish the exact-key watch before the caller re-reads terminal state.
    pub async fn watch_task(
        &self,
        storage_key: &str,
        task_id: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<()>>> {
        use futures::StreamExt;
        let (_, uuid) = parse_task_id(task_id)?;
        let key = a2a_task_key(storage_key, uuid);
        Ok(self
            .store
            .kv_store()
            .watch(key)
            .await?
            .map(|entry| entry.map(|_| ()).map_err(anyhow::Error::from))
            .boxed())
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
            stream_seq: 0,
            created_at: now,
            updated_at: now,
        };
        let payload = serde_json::to_vec(&record)?;
        // Index first: a crash can leave a missing record, never an undiscoverable task.
        // The first CAS create migrates any legacy records before adding this intent.
        self.add_task_to_index(IndexWrite::creation(storage_key, &record))
            .await?;
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.store.put_a2a_task(&key, payload.into()),
        )
        .await
        .context("task record creation timed out after 30 seconds")??;

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

        let old_status = record.task.status.clone();
        mutate(&mut record);
        record.revision += 1;
        record.updated_at = Utc::now();

        // The JSON revision is task-local; NATS revisions span the entire bucket.
        let payload = serde_json::to_vec(&record)?;
        self.store
            .update_a2a_task(&key, payload.into(), current_revision)
            .await?;

        if old_status != record.task.status {
            // KV already committed. An index failure must not turn a successful
            // status write into a stale-revision failure in the publisher.
            self.repair_index_best_effort(storage_key, &record).await;
        }

        Ok(record)
    }
}
