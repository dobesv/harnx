//! Session-backed A2A methods. Admission survives HTTP disconnects.

use crate::{
    exports::Export,
    identity::{Identity, RequestIdentity},
    input_map::InputLimits,
    runner::SessionRequest,
    store::{TaskAccess, TaskRecord},
};
use a2a_lf::*;
use a2a_server_lf::middleware::ServiceParams;
use futures::StreamExt;
use harnx_runtime::NatsSession;
use std::sync::Arc;

mod admission;
mod backend;
mod errors;
mod listing;
mod methods;
pub mod task_view;
#[cfg(test)]
mod tests;
mod wait;
pub use backend::{Backend, BackendConfig};
pub use errors::PERMISSION_DENIED_CODE;
use errors::{map_error, not_found};

struct Admission {
    snapshot: TaskRecord,
    deduped: bool,
    response_after: Option<u64>,
    direct: bool,
    events: Option<tokio::sync::broadcast::Receiver<crate::runner::A2aEvent>>,
}
impl Admission {
    fn reply_snapshot(snapshot: TaskRecord) -> Self {
        let direct = crate::runner::ends_response(&snapshot.task.status.state);
        Self {
            snapshot,
            deduped: false,
            response_after: None,
            direct,
            events: None,
        }
    }

    fn retry(snapshot: TaskRecord) -> Self {
        Self {
            snapshot,
            deduped: true,
            response_after: None,
            direct: false,
            events: None,
        }
    }
}

#[derive(Clone)]
pub struct HarnxHandler {
    export: Export,
    identity: Identity,
    backend: Arc<Backend>,
    limits: InputLimits,
    #[cfg(test)]
    cancel_snapshot_gate: Option<Arc<tests::CancelSnapshotGate>>,
}
impl HarnxHandler {
    pub fn new(
        export: Export,
        identity: Identity,
        backend: Arc<Backend>,
        limits: InputLimits,
    ) -> Self {
        Self {
            export,
            identity,
            backend,
            limits,
            #[cfg(test)]
            cancel_snapshot_gate: None,
        }
    }
    async fn owner(&self, params: &ServiceParams) -> Result<RequestIdentity, A2AError> {
        let owner = self
            .identity
            .resolve_request_params_verified(params)
            .await?;
        if let Some(rules) = self.backend.store.access_rules() {
            if owner.principal.user_id().is_none() {
                return Err(crate::identity::missing_identity());
            }
            let caller = owner.caller();
            if !rules.can_see_agent(&self.export.agent_ref(), caller.view()) {
                return Err(not_found());
            }
        }
        Ok(owner)
    }
    async fn session(
        &self,
        owner: &RequestIdentity,
        context: Option<&str>,
    ) -> Result<NatsSession, A2AError> {
        self.backend
            .runner
            .session(SessionRequest {
                export: &self.export,
                owner,
                local_id: context,
                global_config: &self.backend.config,
                activation_route: self.backend.session_route().await.map_err(map_error)?,
                // Runtime interrupts mutate this flag; don't share it with shutdown.
                abort: harnx_core::abort::create_abort_signal(),
            })
            .await
            .map_err(map_error)
    }
    async fn task(&self, owner: &RequestIdentity, id: &str) -> Result<TaskRecord, A2AError> {
        let record = self
            .backend
            .store
            .get_task_for_export(&self.export, owner, id)
            .await
            .map_err(map_error)?
            .ok_or_else(not_found)?;
        Ok(self.backend.runner.live_record(&self.export, record).await)
    }
    async fn reconcile(
        &self,
        owner: &RequestIdentity,
        record: TaskRecord,
    ) -> Result<TaskRecord, A2AError> {
        if record.task.status.state.is_terminal() {
            return Ok(record);
        }
        // Parked approvals need no foreground claim; Working continuations still
        // need the shared orphan recovery path when their owner disappears.
        if record.task.status.state == TaskState::InputRequired
            && self.hitl_task(owner, &record).await?
        {
            return self.task(owner, &record.task.id).await;
        }
        let session = self.session(owner, Some(&record.task.context_id)).await?;
        self.backend
            .runner
            .reconcile_orphan(
                TaskAccess {
                    export: &self.export,
                    owner,
                    task_id: &record.task.id,
                },
                &session,
            )
            .await
            .map_err(map_error)
    }
    async fn hitl_task(
        &self,
        owner: &RequestIdentity,
        record: &TaskRecord,
    ) -> Result<bool, A2AError> {
        let storage = self
            .backend
            .store
            .resolve_context(&self.export, owner, &record.task.context_id)
            .await
            .map_err(map_error)?
            .ok_or_else(not_found)?;
        Ok(self
            .backend
            .store
            .read_context(&storage)
            .await
            .map_err(map_error)?
            .and_then(|c| c.document.state.active)
            .is_some_and(|a| a.snapshot.task.id == record.task.id && a.hitl_checkpoint.is_some()))
    }

    /// Compatibility entry point: wait until the task ends this response,
    /// including InputRequired and AuthRequired (neither ends the invocation).
    pub async fn wait_terminal(
        &self,
        owner: &RequestIdentity,
        task_id: &str,
    ) -> Result<TaskRecord, A2AError> {
        self.wait_response(owner, task_id, None).await
    }

    /// Wait without owning the turn. Authorization precedes every result.
    ///
    /// For an accepted reply, pass the challenged snapshot's `stream_seq` as
    /// `after`. Interrupted snapshots must have a strictly newer cursor; true
    /// terminal states always win, even below that cursor. Capture it before
    /// recording/delivering the decision, not after publishing Working.
    /// This cursor is task-local, not a KV revision or a runtime request sequence.
    /// Marked waits require a coordinated task with a durable event cursor.
    pub async fn wait_response(
        &self,
        owner: &RequestIdentity,
        task_id: &str,
        after: Option<u64>,
    ) -> Result<TaskRecord, A2AError> {
        let record = self.task(owner, task_id).await?;
        // Dropping a waiter never owns or cancels the detached turn.
        if wait::reached_boundary(&record, after) {
            return Ok(record);
        }
        let storage = harnx_core::session_identity::session_key(
            Some(&self.export.agent),
            &record.task.context_id,
        );
        if self
            .backend
            .store
            .read_context(&storage)
            .await
            .map_err(map_error)?
            .is_some()
        {
            return self.wait_shared_boundary(owner, task_id, after).await;
        }
        self.wait_legacy_boundary(owner, record, after).await
    }

    async fn resolve_send_task(
        &self,
        owner: &RequestIdentity,
        admission: Admission,
        immediate: bool,
    ) -> Result<Task, A2AError> {
        let record = if admission.deduped && !immediate {
            self.reconcile(owner, admission.snapshot).await?
        } else {
            admission.snapshot
        };
        if immediate || admission.direct {
            Ok(record.task)
        } else {
            let waited = self
                .wait_response(owner, &record.task.id, admission.response_after)
                .await?;
            Ok(waited.task)
        }
    }

    async fn cancel_hitl_if_active(
        &self,
        owner: &RequestIdentity,
        task_id: &str,
        initial: &TaskRecord,
    ) -> Result<Option<Task>, A2AError> {
        if initial.task.status.state.is_terminal() {
            return Ok(None);
        }
        if !self.hitl_task(owner, initial).await? {
            return Ok(None);
        }
        let session = self.session(owner, Some(&initial.task.context_id)).await?;
        let task = self
            .backend
            .runner
            .cancel_hitl_task(
                TaskAccess {
                    export: &self.export,
                    owner,
                    task_id,
                },
                &session,
            )
            .await
            .map_err(map_error)?
            .task;
        Ok(Some(task))
    }

    async fn cancel_standard_task(
        &self,
        owner: &RequestIdentity,
        task_id: &str,
        initial: TaskRecord,
    ) -> Result<Task, A2AError> {
        let initially_terminal = initial.task.status.state.is_terminal();
        let record = self.reconcile(owner, initial).await?;
        // The worker can complete mid-reconcile: we read a Working snapshot but the reconcile
        // sees Completed. Return the completed task instead of task_not_cancelable.
        // Verified by nats_http_cancel_returns_completed_when_completion_wins_after_snapshot.
        if !initially_terminal && record.task.status.state == TaskState::Completed {
            return Ok(record.task);
        }
        if record.task.status.state.is_terminal() {
            return Err(A2AError::task_not_cancelable(&record.task.id));
        }
        Ok(self
            .backend
            .runner
            .cancel_task(&self.export, owner, task_id)
            .await
            .map_err(map_error)?
            .task)
    }

    async fn open_reconciled_stream(
        &self,
        owner: &RequestIdentity,
        snapshot: TaskRecord,
    ) -> Result<
        (
            TaskRecord,
            Option<tokio::sync::broadcast::Receiver<crate::runner::A2aEvent>>,
        ),
        A2AError,
    > {
        let record = self.reconcile(owner, snapshot).await?;
        self.backend
            .runner
            .stream_snapshot(&self.export, owner, &record.task.id)
            .await
            .map_err(map_error)
    }
}

/// The compatibility layer maps legacy blocking to returnImmediately before decode.
/// This hook reads the normalized configuration for unary sends and dedupe retries.
pub fn return_immediately(configuration: Option<&SendMessageConfiguration>) -> bool {
    configuration
        .and_then(|configuration| configuration.return_immediately)
        .unwrap_or(false)
}
