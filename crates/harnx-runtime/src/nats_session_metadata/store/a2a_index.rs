//! Per-session task index for efficient listing and reconciliation.
//!
//! Keys use `sessions/{storage_key}/a2a/index` under the session GC prefix.
//! The index is maintained under CAS on task creation and terminal transitions.
//! It enables ListTasks and reconcile_context to avoid full-bucket `keys()` scans.

use super::session_prefix;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Key for the per-session A2A task index.
/// Format: sessions/{storage_key}/a2a/index
pub fn a2a_task_index_key(storage_key: &str) -> String {
    format!("{}/a2a/index", session_prefix(storage_key))
}

/// Normalized task state for index storage.
/// Uses a proto-independent representation to avoid `a2a_lf` dependency in harnx-runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskState {
    Unspecified,
    Submitted,
    AuthRequired,
    Rejected,
    Working,
    InputRequired,
    Completed,
    Canceled,
    Failed,
}

impl TaskState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Canceled | Self::Failed | Self::Rejected
        )
    }
}

/// An entry in the per-session task index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskIndexEntry {
    /// Full task ID: `{local_id}.{uuid}`.
    pub task_id: String,
    /// Current task state.
    pub state: TaskState,
    /// Task-local durable revision, preventing delayed repairs from reverting newer metadata.
    #[serde(default)]
    pub task_revision: u64,
    /// When this task was last updated.
    pub updated_at: DateTime<Utc>,
    /// Status timestamp from the task status, if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_timestamp: Option<DateTime<Utc>>,
}

/// The per-session task index, stored under `sessions/{storage_key}/a2a/index`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskIndex {
    pub entries: Vec<TaskIndexEntry>,
}

impl TaskIndex {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Add or update a task entry, maintaining the sorted invariant by `task_id`.
    /// Returns the index position.
    pub fn add(&mut self, entry: TaskIndexEntry) -> usize {
        match self
            .entries
            .binary_search_by(|e| e.task_id.as_str().cmp(entry.task_id.as_str()))
        {
            Ok(pos) => {
                if entry.task_revision >= self.entries[pos].task_revision {
                    self.entries[pos] = entry;
                }
                pos
            }
            Err(pos) => {
                self.entries.insert(pos, entry);
                pos
            }
        }
    }

    /// Ensure the entries are sorted by `task_id`.
    pub fn sort(&mut self) {
        self.entries.sort_by(|a, b| a.task_id.cmp(&b.task_id));
    }

    /// Remove a task entry by `task_id`, maintaining the sorted invariant.
    pub fn remove(&mut self, task_id: &str) -> Option<TaskIndexEntry> {
        match self
            .entries
            .binary_search_by(|e| e.task_id.as_str().cmp(task_id))
        {
            Ok(pos) => Some(self.entries.remove(pos)),
            Err(_) => None,
        }
    }

    /// Find an entry by `task_id`.
    pub fn get(&self, task_id: &str) -> Option<&TaskIndexEntry> {
        self.entries
            .binary_search_by(|e| e.task_id.as_str().cmp(task_id))
            .ok()
            .map(|pos| &self.entries[pos])
    }
}

impl TaskIndexEntry {
    pub fn is_expired(&self, now: DateTime<Utc>, grace: std::time::Duration) -> bool {
        (now - self.updated_at)
            .to_std()
            .is_ok_and(|age| age >= grace)
    }

    /// Create a new index entry from task metadata.
    pub fn new(task_id: String, state: TaskState, created_at: DateTime<Utc>) -> Self {
        Self::with_status_timestamp(task_id, state, created_at, None)
    }

    /// Create a new index entry with an explicit status timestamp.
    pub fn with_status_timestamp(
        task_id: String,
        state: TaskState,
        created_at: DateTime<Utc>,
        status_timestamp: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            task_id,
            state,
            task_revision: 0,
            updated_at: created_at,
            status_timestamp,
        }
    }
}
