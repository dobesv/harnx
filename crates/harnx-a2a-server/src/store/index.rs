//! Per-session task index migration, revision-fenced mutation and cleanup.
use super::{parse_task_id, A2aStore, TaskRecord};
use crate::{exports::Export, identity::RequestIdentity};
use anyhow::{bail, ensure, Context, Result};
pub use harnx_runtime::nats_session_metadata::TaskState as IndexState;
use harnx_runtime::nats_session_metadata::{
    is_cas_conflict, TaskIndex, TaskIndexEntry, CAS_RETRY_LIMIT,
};

/// Convert a2a_lf TaskState to index TaskState
pub fn to_index_state(state: a2a_lf::TaskState) -> IndexState {
    match state {
        a2a_lf::TaskState::Submitted => IndexState::Submitted,
        a2a_lf::TaskState::Working => IndexState::Working,
        a2a_lf::TaskState::InputRequired => IndexState::InputRequired,
        a2a_lf::TaskState::Completed => IndexState::Completed,
        a2a_lf::TaskState::Canceled => IndexState::Canceled,
        a2a_lf::TaskState::Failed => IndexState::Failed,
        a2a_lf::TaskState::Unspecified => IndexState::Unspecified,
        a2a_lf::TaskState::AuthRequired => IndexState::AuthRequired,
        a2a_lf::TaskState::Rejected => IndexState::Rejected,
    }
}

/// Grace for index-first creations whose task record never landed.
pub const DANGLING_TASK_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(300);

fn index_entry(record: &TaskRecord) -> TaskIndexEntry {
    let mut entry = TaskIndexEntry::with_status_timestamp(
        record.task.id.clone(),
        to_index_state(record.task.status.state.clone()),
        record.created_at,
        record.task.status.timestamp,
    );
    entry.task_revision = record.revision;
    entry.updated_at = record.updated_at;
    entry
}

impl A2aStore {
    /// Return the storage key and task index for this context, migrating legacy sessions if missing.
    pub async fn list_task_index(
        &self,
        export: &Export,
        owner: &RequestIdentity,
        local_id: &str,
    ) -> Result<Option<(String, TaskIndex)>> {
        let Some(key) = self.resolve_context(export, owner, local_id).await? else {
            return Ok(None);
        };
        let (index, _) = self.get_or_migrate_index(&key, Some(local_id)).await?;
        Ok(Some((key, index)))
    }

    /// Get the task index for a session, performing a one-time migration CAS create if missing.
    /// If the index is missing, any existing legacy task records are included in the created index.
    pub async fn get_or_migrate_index(
        &self,
        storage_key: &str,
        local_id: Option<&str>,
    ) -> Result<(TaskIndex, u64)> {
        for attempt in 0..CAS_RETRY_LIMIT {
            match self.index_snapshot(storage_key, local_id).await {
                Ok(snapshot) => return Ok(snapshot),
                Err(error) if retryable(&error, attempt) => tokio::task::yield_now().await,
                Err(error) => {
                    tracing::warn!(%storage_key, %error, "failed to migrate legacy tasks to index");
                    return Err(error);
                }
            }
        }
        tracing::warn!(%storage_key, "get_or_migrate_index exceeded CAS retry limit after {CAS_RETRY_LIMIT} attempts");
        bail!("get_or_migrate_index exceeded retry limit after {CAS_RETRY_LIMIT} attempts")
    }

    async fn index_snapshot(
        &self,
        storage_key: &str,
        local_id: Option<&str>,
    ) -> Result<(TaskIndex, u64)> {
        if let Some(snapshot) = self.store.get_a2a_task_index(storage_key).await? {
            return Ok(snapshot);
        }
        self.create_legacy_index(storage_key, local_id).await
    }

    async fn create_legacy_index(
        &self,
        storage_key: &str,
        local_id: Option<&str>,
    ) -> Result<(TaskIndex, u64)> {
        let records = self.list_tasks_legacy(storage_key, local_id).await?;
        let mut index = TaskIndex::new();
        for record in &records {
            index.add(index_entry(record));
        }
        let revision = self
            .store
            .put_a2a_task_index(storage_key, &index, None)
            .await?;
        tracing::info!(%storage_key, count = records.len(), "migrated legacy tasks to index");
        Ok((index, revision))
    }

    /// List only this authorized context's active (non-terminal) task entries.
    pub async fn list_non_terminal_entries(
        &self,
        storage_key: &str,
    ) -> Result<Vec<TaskIndexEntry>> {
        let (index, _) = self.get_or_migrate_index(storage_key, None).await?;
        Ok(index
            .entries
            .into_iter()
            .filter(|s| !s.state.is_terminal())
            .collect())
    }

    /// List only this authorized context's active (non-terminal) task IDs.
    /// Uses the task index for efficiency, migrating legacy sessions if missing.
    pub async fn list_non_terminal_tasks(&self, storage_key: &str) -> Result<Vec<String>> {
        let entries = self.list_non_terminal_entries(storage_key).await?;
        Ok(entries.into_iter().map(|e| e.task_id).collect())
    }

    /// Clean up an expired dangling index entry under CAS fence and point recheck.
    /// Returns true if removed, false if aborted due to fence or record presence.
    pub async fn cleanup_expired_task_index_entry(
        &self,
        storage_key: &str,
        task_id: &str,
        expected_revision: u64,
    ) -> Result<bool> {
        let target = CleanupTarget::new(storage_key, task_id, expected_revision)?;
        if self
            .record_blocks_cleanup(&target, "aborting cleanup: record exists")
            .await?
        {
            return Ok(false);
        }
        for attempt in 0..CAS_RETRY_LIMIT {
            if let Some(removed) = self.cleanup_attempt(&target, attempt).await? {
                return Ok(removed);
            }
        }
        tracing::warn!(%task_id, %storage_key, "CAS retry limit exhausted removing expired index entry");
        Ok(false)
    }

    async fn cleanup_attempt(
        &self,
        target: &CleanupTarget<'_>,
        attempt: usize,
    ) -> Result<Option<bool>> {
        let Some((mut index, revision)) = self.cleanup_candidate(target).await? else {
            return Ok(Some(false));
        };
        if self
            .record_blocks_cleanup(target, "aborting cleanup: record appeared")
            .await?
        {
            return Ok(Some(false));
        }
        index.remove(target.task_id);
        match self
            .store
            .put_a2a_task_index(target.storage_key, &index, Some(revision))
            .await
        {
            Ok(_) => {
                tracing::info!(task_id = %target.task_id, storage_key = %target.storage_key, "removed expired dangling index entry");
                Ok(Some(true))
            }
            Err(error) => {
                self.retry_cleanup(target, error, attempt).await?;
                Ok(None)
            }
        }
    }

    async fn record_blocks_cleanup(
        &self,
        target: &CleanupTarget<'_>,
        reason: &str,
    ) -> Result<bool> {
        let exists = self
            .get_task(target.storage_key, target.task_id)
            .await?
            .is_some();
        if exists {
            tracing::debug!(task_id = %target.task_id, storage_key = %target.storage_key, "{reason}");
        }
        Ok(exists)
    }

    async fn cleanup_candidate(
        &self,
        target: &CleanupTarget<'_>,
    ) -> Result<Option<(TaskIndex, u64)>> {
        let Some(snapshot) = self.store.get_a2a_task_index(target.storage_key).await? else {
            return Ok(None);
        };
        let Some(entry) = snapshot
            .0
            .entries
            .iter()
            .find(|entry| entry.task_id == target.task_id)
        else {
            return Ok(None);
        };
        if !target.accepts(entry) {
            tracing::debug!(task_id = %target.task_id, current_revision = entry.task_revision,
                expected_revision = target.expected_revision, state = ?entry.state,
                "skipping cleanup: index entry was updated or transitioned");
            return Ok(None);
        }
        Ok(Some(snapshot))
    }

    async fn retry_cleanup(
        &self,
        target: &CleanupTarget<'_>,
        error: anyhow::Error,
        attempt: usize,
    ) -> Result<()> {
        if retryable(&error, attempt) {
            tokio::task::yield_now().await;
            return Ok(());
        }
        tracing::warn!(task_id = %target.task_id, storage_key = %target.storage_key, %error, "failed to remove expired dangling index entry");
        Err(error)
    }

    /// Repair metadata from an authoritative point read, preserving exact status timestamps.
    pub async fn repair_index(&self, storage_key: &str, record: &TaskRecord) -> Result<()> {
        let authoritative;
        let record = if self.read_context(storage_key).await?.is_some() {
            authoritative = self
                .get_task(storage_key, &record.task.id)
                .await?
                .context("index repair has no authoritative task")?;
            &authoritative
        } else {
            record
        };
        self.add_task_to_index(IndexWrite::repair(storage_key, record))
            .await?;
        tracing::debug!(task_id = %record.task.id, revision = record.revision, %storage_key, "reconciled task index metadata");
        Ok(())
    }

    pub async fn repair_index_best_effort(&self, storage_key: &str, record: &TaskRecord) {
        if let Err(error) = self.repair_index(storage_key, record).await {
            tracing::warn!(task_id = %record.task.id, %error, "A2A index repair deferred to next point read");
        }
    }

    /// Legacy scan for sessions without an index.
    async fn list_tasks_legacy(
        &self,
        storage_key: &str,
        local_id: Option<&str>,
    ) -> Result<Vec<TaskRecord>> {
        let mut records = Vec::new();
        for (bytes, _) in self.store.list_a2a_tasks(storage_key).await? {
            let record: TaskRecord = serde_json::from_slice(&bytes)?;
            if let Some(expected_id) = local_id {
                ensure!(
                    record.version == 1 && record.task.context_id == expected_id,
                    "invalid task record"
                );
            } else {
                ensure!(record.version == 1, "invalid task record");
            }
            records.push(record);
        }
        records.sort_by(|a, b| a.task.id.cmp(&b.task.id));
        Ok(records)
    }

    pub(super) async fn add_task_to_index(&self, mut write: IndexWrite<'_>) -> Result<()> {
        let mut snapshot = self
            .get_or_migrate_index(write.storage_key, Some(write.local_id))
            .await?;
        for attempt in 0..CAS_RETRY_LIMIT {
            if self
                .index_write_attempt(&mut write, &mut snapshot, attempt)
                .await?
            {
                return Ok(());
            }
            snapshot = self
                .store
                .get_a2a_task_index(write.storage_key)
                .await?
                .context("task index disappeared during mutation")?;
        }
        tracing::warn!(storage_key = %write.storage_key, "add_task_to_index exceeded CAS retry limit after {CAS_RETRY_LIMIT} attempts");
        bail!("add_task_to_index exceeded retry limit after {CAS_RETRY_LIMIT} attempts")
    }

    async fn index_write_attempt(
        &self,
        write: &mut IndexWrite<'_>,
        snapshot: &mut (TaskIndex, u64),
        attempt: usize,
    ) -> Result<bool> {
        let (index, revision) = snapshot;
        if !write.merge(index) {
            return Ok(true);
        }
        match self
            .store
            .put_a2a_task_index(write.storage_key, index, Some(*revision))
            .await
        {
            Ok(_) => Ok(true),
            Err(error) => {
                self.retry_index_write(write, error, attempt).await?;
                Ok(false)
            }
        }
    }

    async fn retry_index_write(
        &self,
        write: &IndexWrite<'_>,
        error: anyhow::Error,
        attempt: usize,
    ) -> Result<()> {
        if retryable(&error, attempt) {
            tokio::task::yield_now().await;
            return Ok(());
        }
        tracing::warn!(storage_key = %write.storage_key, task_id = %write.entry.task_id, attempts = attempt + 1,
            cas_conflict = is_cas_conflict(&error), %error, "A2A index mutation failed");
        Err(error)
    }
}

fn retryable(error: &anyhow::Error, attempt: usize) -> bool {
    is_cas_conflict(error) && attempt + 1 < CAS_RETRY_LIMIT
}

struct CleanupTarget<'a> {
    storage_key: &'a str,
    task_id: &'a str,
    expected_revision: u64,
}
impl<'a> CleanupTarget<'a> {
    fn new(storage_key: &'a str, task_id: &'a str, expected_revision: u64) -> Result<Self> {
        parse_task_id(task_id)?;
        Ok(Self {
            storage_key,
            task_id,
            expected_revision,
        })
    }
    fn accepts(&self, entry: &TaskIndexEntry) -> bool {
        if entry.task_revision != self.expected_revision {
            return false;
        }
        if entry.state.is_terminal() {
            return false;
        }
        entry.is_expired(chrono::Utc::now(), DANGLING_TASK_GRACE_PERIOD)
    }
}

pub(super) struct IndexWrite<'a> {
    storage_key: &'a str,
    local_id: &'a str,
    entry: TaskIndexEntry,
    creation_intent: bool,
}
impl<'a> IndexWrite<'a> {
    pub(super) fn creation(storage_key: &'a str, record: &'a TaskRecord) -> Self {
        Self::from_record(storage_key, record, true)
    }
    fn repair(storage_key: &'a str, record: &'a TaskRecord) -> Self {
        Self::from_record(storage_key, record, false)
    }
    fn from_record(storage_key: &'a str, record: &'a TaskRecord, creation_intent: bool) -> Self {
        Self {
            storage_key,
            local_id: &record.task.context_id,
            entry: index_entry(record),
            creation_intent,
        }
    }
    fn covered_by(&self, current: &TaskIndexEntry) -> bool {
        if current.task_revision > self.entry.task_revision {
            return true;
        }
        *current == self.entry
    }
    fn merge(&mut self, index: &mut TaskIndex) -> bool {
        if index
            .entries
            .iter()
            .filter(|entry| entry.task_id == self.entry.task_id)
            .any(|entry| self.covered_by(entry))
        {
            return false;
        }
        if self.creation_intent {
            // Start grace after a potentially slow legacy migration, at the write.
            self.entry.updated_at = chrono::Utc::now();
        }
        index.add(self.entry.clone());
        true
    }
}
