//! Index candidate repair and page-only record projection.
use super::{task_view, HarnxHandler};
use crate::{
    identity::Principal,
    store::{TaskRecord, DANGLING_TASK_GRACE_PERIOD},
};
use a2a_lf::{A2AError, ListTasksRequest, ListTasksResponse};
use harnx_runtime::nats_session_metadata::TaskIndexEntry;
use std::collections::HashMap;

#[derive(Clone, Copy)]
pub(super) struct ListingScope<'a> {
    pub owner: &'a Principal,
    pub key: &'a str,
    pub context: &'a str,
}

struct Candidate {
    entry: TaskIndexEntry,
    record: Option<TaskRecord>,
}

pub(super) struct Candidates {
    entries: Vec<TaskIndexEntry>,
    loaded: HashMap<String, TaskRecord>,
}

impl HarnxHandler {
    async fn list_record(
        &self,
        scope: ListingScope<'_>,
        entry: &TaskIndexEntry,
    ) -> Result<Option<TaskRecord>, A2AError> {
        let Some(record) = self
            .backend
            .store
            .get_task(scope.key, &entry.task_id)
            .await
            .map_err(super::errors::map_error)?
        else {
            return Ok(None);
        };
        if record.version != 1 || record.task.context_id != scope.context {
            return Err(A2AError::internal("invalid task record"));
        }
        let record = self.reconcile(scope.owner, record).await?;
        let record = self.backend.runner.live_record(&self.export, record).await;
        if crate::store::to_index_state(record.task.status.state.clone()) != entry.state
            || record.task.status.timestamp != entry.status_timestamp
        {
            self.backend
                .store
                .repair_index_best_effort(scope.key, &record)
                .await;
        }
        Ok(Some(record))
    }

    pub(super) async fn list_candidates(
        &self,
        scope: ListingScope<'_>,
        entries: Vec<TaskIndexEntry>,
        req: &ListTasksRequest,
    ) -> Result<Candidates, A2AError> {
        let mut candidates = Candidates {
            entries: Vec::with_capacity(entries.len()),
            loaded: HashMap::new(),
        };
        for entry in entries {
            let Some(candidate) = self.read_candidate(scope, entry, req).await? else {
                continue;
            };
            candidates.consider(candidate, req);
        }
        Ok(candidates)
    }

    async fn read_candidate(
        &self,
        scope: ListingScope<'_>,
        mut entry: TaskIndexEntry,
        req: &ListTasksRequest,
    ) -> Result<Option<Candidate>, A2AError> {
        if !requires_record(&entry, req) {
            return Ok(Some(Candidate {
                entry,
                record: None,
            }));
        }
        let Some(record) = self.list_record(scope, &entry).await? else {
            self.handle_missing_entry(scope.key, &entry).await;
            return Ok(None);
        };
        entry.state = crate::store::to_index_state(record.task.status.state.clone());
        entry.status_timestamp = record.task.status.timestamp;
        Ok(Some(Candidate {
            entry,
            record: Some(record),
        }))
    }

    pub(super) async fn handle_missing_entry(&self, key: &str, entry: &TaskIndexEntry) {
        if !entry.is_expired(chrono::Utc::now(), DANGLING_TASK_GRACE_PERIOD) {
            tracing::debug!(task_id = %entry.task_id, "skipped missing record within grace period");
            return;
        }
        if let Err(error) = self
            .backend
            .store
            .cleanup_expired_task_index_entry(key, &entry.task_id, entry.task_revision)
            .await
        {
            tracing::warn!(task_id = %entry.task_id, storage_key = %key, %error,
                "dangling index cleanup deferred; continuing ListTasks");
        }
    }

    async fn page_record(
        &self,
        scope: ListingScope<'_>,
        entry: &TaskIndexEntry,
        loaded: &mut HashMap<String, TaskRecord>,
    ) -> Result<Option<TaskRecord>, A2AError> {
        if let Some(record) = loaded.remove(&entry.task_id) {
            return Ok(Some(record));
        }
        self.list_record(scope, entry).await
    }

    pub(super) async fn list_page(
        &self,
        scope: ListingScope<'_>,
        mut candidates: Candidates,
        req: &ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        let mut position =
            task_view::resolve_offset(&candidates.entries, req.page_token.as_deref())?;
        let page_size = a2a_server_lf::pagination::resolve_page_size(req.page_size);
        let mut tasks = Vec::new();
        while tasks.len() < page_size && position < candidates.entries.len() {
            let entry = &candidates.entries[position];
            let record = self
                .page_record(scope, entry, &mut candidates.loaded)
                .await?;
            if let Some(record) = record {
                let mut task = record.task;
                task_view::project_task(&mut task, req)?;
                tasks.push(task);
                position += 1;
            } else {
                tracing::debug!(task_id = %entry.task_id, "skipping missing record on page");
                candidates.entries.remove(position);
            }
        }
        let next_page_token = if position < candidates.entries.len() {
            tasks.last().map(|task| task.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        Ok(ListTasksResponse {
            tasks,
            next_page_token,
            page_size: page_size as i32,
            total_size: candidates.entries.len() as i32,
        })
    }
}

impl Candidates {
    fn consider(&mut self, candidate: Candidate, req: &ListTasksRequest) {
        if let Some(record) = candidate.record {
            self.loaded.insert(candidate.entry.task_id.clone(), record);
        }
        if task_view::entry_matches(&candidate.entry, req) {
            self.entries.push(candidate.entry);
        }
    }
}

fn requires_record(entry: &TaskIndexEntry, req: &ListTasksRequest) -> bool {
    // Nonterminal reads distinguish active tasks from missing creation intents
    // before counting. Timestamp filters must also repair incomplete metadata.
    if !entry.state.is_terminal() {
        return true;
    }
    if req.status_timestamp_after.is_none() {
        return false;
    }
    entry.status_timestamp.is_none()
}
