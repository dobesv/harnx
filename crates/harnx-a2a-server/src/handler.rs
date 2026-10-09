//! Session-backed A2A methods. Admission survives HTTP disconnects.

use crate::{
    exports::Export,
    identity::{Identity, RequestIdentity},
    input_map::InputLimits,
    runner::{RunnerError, SessionRequest},
    store::{TaskAccess, TaskRecord},
};
use a2a_lf::*;
use a2a_server_lf::{handler::RequestHandler, middleware::ServiceParams};
use async_trait::async_trait;
use futures::{stream::BoxStream, StreamExt};
use harnx_runtime::NatsSession;
use std::sync::Arc;

mod admission;
mod backend;
mod errors;
mod listing;
pub mod task_view;
mod wait;
pub use backend::{Backend, BackendConfig};
pub use errors::PERMISSION_DENIED_CODE;
use errors::{map_error, not_found};
use task_view::{history, validate_history};

struct Admission {
    snapshot: TaskRecord,
    deduped: bool,
}
impl Admission {
    fn retry(snapshot: TaskRecord) -> Self {
        Self {
            snapshot,
            deduped: true,
        }
    }
}

#[derive(Clone)]
pub struct HarnxHandler {
    export: Export,
    identity: Identity,
    backend: Arc<Backend>,
    limits: InputLimits,
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
        }
    }
    fn owner(&self, params: &ServiceParams) -> Result<RequestIdentity, A2AError> {
        let owner = self.identity.resolve_request_params(params)?;
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
    /// Wait for an admitted task without owning its turn. The point read checks
    /// context ownership even when called directly by an in-process client.
    pub async fn wait_terminal(
        &self,
        owner: &RequestIdentity,
        task_id: &str,
    ) -> Result<TaskRecord, A2AError> {
        let record = self.task(owner, task_id).await?;
        // Dropping a waiter never owns or cancels the detached turn.
        if record.task.status.state.is_terminal() {
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
            return self.wait_shared_terminal(owner, task_id).await;
        }
        self.wait_legacy_terminal(owner, record).await
    }
}

/// The compatibility layer maps legacy blocking to returnImmediately before decode.
/// This hook reads the normalized configuration for unary sends and dedupe retries.
pub fn return_immediately(configuration: Option<&SendMessageConfiguration>) -> bool {
    configuration
        .and_then(|configuration| configuration.return_immediately)
        .unwrap_or(false)
}

#[async_trait]
impl RequestHandler for HarnxHandler {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        let owner = self.owner(params)?;
        let immediate = return_immediately(req.configuration.as_ref());
        let (admission, length) = self.admit_request(owner.clone(), req).await?;
        let record = admission.snapshot;
        // Retries keep their task ID and chosen return mode. Reconcile an
        // orphan before a blocking wait, without applying new-turn rejection.
        let record = if admission.deduped && !immediate {
            self.reconcile(&owner, record).await?
        } else {
            record
        };
        let mut task = if immediate {
            record
        } else {
            self.wait_terminal(&owner, &record.task.id).await?
        }
        .task;
        history(&mut task, length)?;
        Ok(SendMessageResponse::Task(task))
    }
    async fn get_task(
        &self,
        params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        let owner = self.owner(params)?;
        let mut task = self
            .reconcile(&owner, self.task(&owner, &req.id).await?)
            .await?
            .task;
        history(&mut task, req.history_length)?;
        Ok(task)
    }
    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        let owner = self.owner(params)?;
        let initial = self.task(&owner, &req.id).await?;
        let initially_terminal = initial.task.status.state.is_terminal();
        let record = self.reconcile(&owner, initial).await?;
        if !initially_terminal && record.task.status.state == TaskState::Completed {
            return Ok(record.task);
        }
        if record.task.status.state.is_terminal() {
            return Err(A2AError::task_not_cancelable(&record.task.id));
        }
        Ok(self
            .backend
            .runner
            .cancel_task(&self.export, &owner, &req.id)
            .await
            .map_err(map_error)?
            .task)
    }
    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        let owner = self.owner(params)?;
        let context = req
            .context_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| A2AError::invalid_params("ListTasks requires contextId"))?;
        validate_history(req.history_length)?;
        let Some((key, index)) = self
            .backend
            .store
            .list_task_index(&self.export, &owner, context)
            .await
            .map_err(map_error)?
        else {
            return Err(not_found());
        };

        let scope = listing::ListingScope {
            owner: &owner,
            key: &key,
            context,
        };
        let candidates = self.list_candidates(scope, index.entries, &req).await?;
        self.list_page(scope, candidates, &req).await
    }
    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let owner = self.owner(params)?;
        let (admission, length) = self.admit_request(owner.clone(), req).await?;
        let record = self.reconcile(&owner, admission.snapshot).await?;
        let (mut snapshot, events) = self
            .backend
            .runner
            .stream_snapshot(&self.export, &owner, &record.task.id)
            .await
            .map_err(map_error)?;
        history(&mut snapshot.task, length)?;
        Ok(crate::sse::task_stream(snapshot, events))
    }
    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let owner = self.owner(params)?;
        let record = self
            .reconcile(&owner, self.task(&owner, &req.id).await?)
            .await?;
        if record.task.status.state.is_terminal() {
            return Err(A2AError::unsupported_operation(
                "terminal tasks cannot be subscribed to",
            ));
        }
        let subscription = self
            .backend
            .runner
            .subscribe(&self.export, &owner, &req.id)
            .await
            .map_err(|error| {
                if matches!(
                    error.downcast_ref::<RunnerError>(),
                    Some(RunnerError::Terminal)
                ) {
                    A2AError::unsupported_operation("terminal tasks cannot be subscribed to")
                } else {
                    map_error(error)
                }
            })?;
        Ok(crate::sse::task_stream(
            subscription.snapshot,
            Some(subscription.events),
        ))
    }
    async fn create_push_config(
        &self,
        params: &ServiceParams,
        _req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.owner(params)?;
        Err(A2AError::push_notification_not_supported())
    }
    async fn get_push_config(
        &self,
        params: &ServiceParams,
        _req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.owner(params)?;
        Err(A2AError::push_notification_not_supported())
    }
    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        _req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        self.owner(params)?;
        Err(A2AError::push_notification_not_supported())
    }
    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        _req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        self.owner(params)?;
        Err(A2AError::push_notification_not_supported())
    }
    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        _req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        self.owner(params)?;
        Err(A2AError::unsupported_operation(
            "extended agent cards are not supported",
        ))
    }
}
