//! Local done is a wake hint, not authority after remote takeover.
use super::*;
use crate::runner::ends_response;

pub(super) fn reached_boundary(record: &TaskRecord, after: Option<u64>) -> bool {
    record.task.status.state.is_terminal()
        || (ends_response(&record.task.status.state)
            && after.is_none_or(|sequence| record.stream_seq > sequence))
}

#[derive(Clone, Copy)]
struct BoundaryTarget<'a> {
    record: &'a TaskRecord,
    after: Option<u64>,
}

struct WaitTarget<'a> {
    owner: &'a RequestIdentity,
    task_id: &'a str,
    after: Option<u64>,
}

struct SharedWaitState {
    events: tokio::sync::broadcast::Receiver<crate::runner::A2aEvent>,
    done: Option<tokio::sync::watch::Receiver<bool>>,
}

impl HarnxHandler {
    async fn wait_local_completion(
        &self,
        owner: &RequestIdentity,
        target: BoundaryTarget<'_>,
        done: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<Option<TaskRecord>, A2AError> {
        let BoundaryTarget { record, after } = target;
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
            if reached_boundary(&latest, after) {
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

    async fn check_shared_event_boundary(
        &self,
        owner: &RequestIdentity,
        task_id: &str,
        after: Option<u64>,
    ) -> Result<Option<TaskRecord>, A2AError> {
        let record = self.task(owner, task_id).await?;
        if reached_boundary(&record, after) {
            Ok(Some(record))
        } else {
            Ok(None)
        }
    }

    async fn resolve_shared_completion(
        &self,
        snapshot: &TaskRecord,
        after: Option<u64>,
    ) -> Option<tokio::sync::watch::Receiver<bool>> {
        if after.is_some_and(|sequence| snapshot.stream_seq <= sequence) {
            None
        } else {
            self.backend.runner.completion(&self.export, snapshot).await
        }
    }

    async fn prepare_shared_wait(
        &self,
        owner: &RequestIdentity,
        task_id: &str,
        after: Option<u64>,
    ) -> Result<Result<TaskRecord, SharedWaitState>, A2AError> {
        self.reconcile(owner, self.task(owner, task_id).await?)
            .await?;
        let (snapshot, events) = self
            .backend
            .runner
            .stream_snapshot(&self.export, owner, task_id)
            .await
            .map_err(map_error)?;
        if reached_boundary(&snapshot, after) {
            return Ok(Ok(snapshot));
        }
        let events = events.ok_or_else(|| A2AError::internal("task reader missing"))?;
        let done = self.resolve_shared_completion(&snapshot, after).await;
        if done.is_some() {
            tracing::info!(%task_id, "waiting on local task completion");
        } else {
            tracing::info!(%task_id, "waiting for task via JetStream");
        }
        Ok(Err(SharedWaitState { events, done }))
    }

    async fn run_shared_wait_loop(
        &self,
        target: WaitTarget<'_>,
        state: &mut SharedWaitState,
    ) -> Result<TaskRecord, A2AError> {
        let WaitTarget {
            owner,
            task_id,
            after,
        } = target;
        loop {
            tokio::select! {
                event = state.events.recv() => {
                    let event = event.map_err(|_| A2AError::internal("task stream interrupted; reconnect"))?;
                    if event.ends_response() {
                        if let Some(record) = self.check_shared_event_boundary(owner, task_id, after).await? {
                            return Ok(record);
                        }
                    }
                }
                _ = wait_done(&mut state.done) => {
                    let record = self.reconcile(owner, self.task(owner, task_id).await?).await?;
                    if reached_boundary(&record, after) { return Ok(record); }
                    return Err(A2AError::internal("task stopped without response-boundary persistence"));
                }
            }
        }
    }

    pub(super) async fn wait_shared_boundary(
        &self,
        owner: &RequestIdentity,
        task_id: &str,
        after: Option<u64>,
    ) -> Result<TaskRecord, A2AError> {
        let mut state = match self.prepare_shared_wait(owner, task_id, after).await? {
            Ok(ready) => return Ok(ready),
            Err(state) => state,
        };
        let target = WaitTarget {
            owner,
            task_id,
            after,
        };
        self.run_shared_wait_loop(target, &mut state).await
    }

    async fn wait_legacy_local(
        &self,
        owner: &RequestIdentity,
        target: BoundaryTarget<'_>,
        mut done: tokio::sync::watch::Receiver<bool>,
    ) -> Result<TaskRecord, A2AError> {
        let task_id = &target.record.task.id;
        tracing::info!(%task_id, "waiting on local task completion");
        if let Some(terminal) = self.wait_local_completion(owner, target, &mut done).await? {
            return Ok(terminal);
        }
        let record = self.task(owner, task_id).await?;
        if reached_boundary(&record, target.after) {
            return Ok(record);
        }
        let record = self.reconcile(owner, record).await?;
        if reached_boundary(&record, target.after) {
            Ok(record)
        } else {
            Err(A2AError::internal(
                "task stopped without response-boundary persistence",
            ))
        }
    }

    async fn next_legacy_watch_record(
        &self,
        owner: &RequestIdentity,
        task_id: &str,
        changes: &mut (impl futures::Stream<Item = anyhow::Result<()>> + Unpin),
    ) -> Result<TaskRecord, A2AError> {
        changes
            .next()
            .await
            .ok_or_else(|| A2AError::internal("task watch closed"))?
            .map_err(map_error)?;
        self.reconcile(owner, self.task(owner, task_id).await?)
            .await
    }

    async fn wait_legacy_watch(
        &self,
        owner: &RequestIdentity,
        record: TaskRecord,
        after: Option<u64>,
    ) -> Result<TaskRecord, A2AError> {
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
        let mut record = self
            .reconcile(owner, self.task(owner, &record.task.id).await?)
            .await?;
        while !reached_boundary(&record, after) {
            record = self
                .next_legacy_watch_record(owner, &record.task.id, &mut changes)
                .await?;
        }
        Ok(record)
    }

    pub(super) async fn wait_legacy_boundary(
        &self,
        owner: &RequestIdentity,
        record: TaskRecord,
        after: Option<u64>,
    ) -> Result<TaskRecord, A2AError> {
        if after.is_some() {
            return Err(A2AError::unsupported_operation(
                "response cursor requires a coordinated task",
            ));
        }
        if let Some(done) = self.backend.runner.completion(&self.export, &record).await {
            let target = BoundaryTarget {
                record: &record,
                after,
            };
            return self.wait_legacy_local(owner, target, done).await;
        }
        self.wait_legacy_watch(owner, record, after).await
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
