//! Local done is a wake hint, not authority after remote takeover.
use super::*;

impl HarnxHandler {
    pub(super) async fn wait_local_completion(
        &self,
        owner: &RequestIdentity,
        record: &TaskRecord,
        done: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<Option<TaskRecord>, A2AError> {
        let storage = harnx_core::session_identity::session_key(
            Some(&self.export.agent),
            &record.task.context_id,
        );
        let mut changes = self
            .backend
            .store
            .watch_task(&storage, &record.task.id)
            .await
            .map_err(map_error)?;
        // Subscribe before the point read so takeover/terminal CAS in the gap
        // can't leave this waiter on a former owner's never-finished local watch.
        loop {
            let latest = self.task(owner, &record.task.id).await?;
            if latest.task.status.state.is_terminal() {
                return Ok(Some(latest));
            }
            tokio::select! {
                _ = done.wait_for(|finished| *finished) => return Ok(None),
                change = changes.next() => {
                    change.ok_or_else(|| A2AError::internal("task watch closed"))?.map_err(map_error)?;
                }
            }
        }
    }
}

impl HarnxHandler {
    pub(super) async fn wait_shared_terminal(
        &self,
        owner: &RequestIdentity,
        task_id: &str,
    ) -> Result<TaskRecord, A2AError> {
        // Recovery is a preflight, never the snapshot used for handoff. A stopped
        // local writer with unresolved KV state still needs foreground settlement.
        self.reconcile(owner, self.task(owner, task_id).await?)
            .await?;
        let (snapshot, events) = self
            .backend
            .runner
            .stream_snapshot(&self.export, owner, task_id)
            .await
            .map_err(map_error)?;
        if snapshot.task.status.state.is_terminal() {
            return Ok(snapshot);
        }
        let mut events = events.ok_or_else(|| A2AError::internal("task reader missing"))?;
        let mut done = self
            .backend
            .runner
            .completion(&self.export, &snapshot)
            .await;
        if done.is_some() {
            tracing::info!(%task_id, "waiting on local task completion");
        } else {
            tracing::info!(%task_id, "waiting for task via JetStream");
        }
        loop {
            tokio::select! {
                event = events.recv() => {
                    let event = event.map_err(|_| A2AError::internal("task stream interrupted; reconnect"))?;
                    if event.is_terminal() { return self.task(owner, task_id).await; }
                }
                _ = wait_done(&mut done) => {
                    // A local writer can finish with unresolved persistence. Keep
                    // the old bounded reconcile/error behavior, not a hung reader.
                    let record = self.reconcile(owner, self.task(owner, task_id).await?).await?;
                    if record.task.status.state.is_terminal() { return Ok(record); }
                    return Err(A2AError::internal("task stopped without terminal persistence"));
                }
            }
        }
    }
}
async fn wait_done(done: &mut Option<tokio::sync::watch::Receiver<bool>>) {
    match done {
        Some(done) => {
            let _ = done.wait_for(|finished| *finished).await;
        }
        None => futures::future::pending::<()>().await,
    }
}

impl HarnxHandler {
    pub(super) async fn wait_legacy_terminal(
        &self,
        owner: &RequestIdentity,
        mut record: TaskRecord,
    ) -> Result<TaskRecord, A2AError> {
        if let Some(mut done) = self.backend.runner.completion(&self.export, &record).await {
            tracing::info!(task_id = %record.task.id, "waiting on local task completion");
            // A dropped supervisor must fall through to orphan reconciliation.
            if let Some(terminal) = self
                .wait_local_completion(owner, &record, &mut done)
                .await?
            {
                return Ok(terminal);
            }
            record = self.task(owner, &record.task.id).await?;
            if record.task.status.state.is_terminal() {
                return Ok(record);
            }
            // done means no local writer remains. A KV outage may have prevented
            // final persistence; fence it or fail instead of waiting indefinitely.
            record = self.reconcile(owner, record).await?;
            return if record.task.status.state.is_terminal() {
                Ok(record)
            } else {
                Err(A2AError::internal(
                    "task stopped without terminal persistence",
                ))
            };
        }
        let key = self
            .backend
            .store
            .resolve_context(&self.export, owner, &record.task.context_id)
            .await
            .map_err(map_error)?
            .ok_or_else(not_found)?;
        tracing::info!(task_id = %record.task.id, "waiting for task via KV watch");
        let mut changes = self
            .backend
            .store
            .watch_task(&key, &record.task.id)
            .await
            .map_err(map_error)?;
        // Completion may precede watch creation. Re-read after subscribing, and
        // fence abandoned tasks rather than waiting forever on a stopped writer.
        record = self
            .reconcile(owner, self.task(owner, &record.task.id).await?)
            .await?;
        while !record.task.status.state.is_terminal() {
            changes
                .next()
                .await
                .ok_or_else(|| A2AError::internal("task watch closed"))?
                .map_err(map_error)?;
            record = self
                .reconcile(owner, self.task(owner, &record.task.id).await?)
                .await?;
        }
        Ok(record)
    }
}
