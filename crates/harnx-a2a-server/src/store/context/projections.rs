//! Terminal archives are create-only projections. Active reads always prefer
//! the context document, so delayed archive/index repair cannot change authority.
use super::super::{parse_task_id, TaskRecord};
use super::*;

fn archive_key(storage_key: &str, task_id: &str) -> Result<String> {
    let (_, uuid) = parse_task_id(task_id)?;
    Ok(format!("sessions/{storage_key}/a2a/archive/{uuid}"))
}

impl A2aStore {
    pub async fn read_terminal_archive(
        &self,
        storage_key: &str,
        task_id: &str,
    ) -> Result<Option<TaskRecord>> {
        let key = archive_key(storage_key, task_id)?;
        let Some(bytes) = leader_reads::get(self.store.kv_store(), &key).await? else {
            return Ok(None);
        };
        let record: TaskRecord = serde_json::from_slice(&bytes)?;
        ensure!(
            record.version == 1
                && record.task.id == task_id
                && record.task.status.state.is_terminal(),
            "invalid terminal archive"
        );
        Ok(Some(record))
    }

    /// A former owner may finish projecting an already committed terminal snapshot;
    /// no owner may replace that snapshot. Receipt/active retirement CAS stays separate.
    pub async fn archive_context_terminal(
        &self,
        storage_key: &str,
        task_id: &str,
    ) -> Result<TaskRecord> {
        // This is a repairable projection, not an owner mutation. Read the
        // immutable committed snapshot, even if its former owner was replaced.
        let current = self
            .read_context(storage_key)
            .await?
            .context("context missing")?;
        let Some(active) = current
            .document
            .state
            .active
            .filter(|active| active.snapshot.task.id == task_id)
        else {
            return self
                .read_terminal_archive(storage_key, task_id)
                .await?
                .context("no matching active task or archive");
        };
        let record = active.snapshot;
        ensure!(
            record.task.status.state.is_terminal(),
            "cannot archive a nonterminal task"
        );
        let key = archive_key(storage_key, &record.task.id)?;
        let payload = serde_json::to_vec(&record)?;
        let kv = self.store.kv_store();
        match kv.update(&key, payload.into(), 0).await {
            Ok(_) => Ok(record),
            Err(error) => {
                let stored = self
                    .read_terminal_archive(storage_key, &record.task.id)
                    .await?
                    .with_context(|| {
                        format!("terminal archive acknowledgement unresolved: {error}")
                    })?;
                ensure!(
                    serde_json::to_value(&stored)? == serde_json::to_value(&record)?,
                    "immutable terminal archive conflict"
                );
                Ok(stored)
            }
        }
    }

    pub(super) async fn validate_new_task(
        &self,
        storage_key: &str,
        old: &ContextState,
        next: &ContextState,
    ) -> Result<()> {
        let Some(active) = &next.active else {
            return Ok(());
        };
        if old
            .active
            .as_ref()
            .is_some_and(|previous| previous.snapshot.task.id == active.snapshot.task.id)
        {
            return Ok(());
        }
        ensure!(
            active.snapshot.revision == 1,
            "new task snapshot revision must be one"
        );
        ensure!(
            self.get_task(storage_key, &active.snapshot.task.id)
                .await?
                .is_none(),
            "terminal task id cannot be reused"
        );
        Ok(())
    }

    pub(super) async fn verify_archive_projection(
        &self,
        storage_key: &str,
        state: &ContextState,
    ) -> Result<()> {
        let Some(active) = state
            .active
            .as_ref()
            .filter(|active| active.projections.archive)
        else {
            return Ok(());
        };
        let archive = self
            .read_terminal_archive(storage_key, &active.snapshot.task.id)
            .await?
            .context("terminal archive not durable")?;
        ensure!(
            serde_json::to_value(&active.snapshot)? == serde_json::to_value(&archive)?,
            "terminal archive projection mismatch"
        );
        Ok(())
    }
}
