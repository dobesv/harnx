//! Session-backed A2A methods. Admission survives HTTP disconnects.

use crate::{
    exports::Export,
    identity::{Identity, Principal},
    input_map::InputLimits,
    runner::{A2aEvent, RunnerError, SessionRequest},
    store::{TaskAccess, TaskRecord},
};
use a2a_lf::*;
use a2a_server_lf::{handler::RequestHandler, middleware::ServiceParams};
use async_trait::async_trait;
use futures::stream::BoxStream;
use harnx_runtime::NatsSession;
use std::{sync::Arc, time::Duration};
use tokio::sync::broadcast;

mod admission;
mod backend;
mod errors;
mod task_view;
pub use backend::{Backend, BackendConfig};
pub use errors::PERMISSION_DENIED_CODE;
use errors::{map_error, not_found};
use task_view::{history, paginate, project_task, task_matches, validate_history};

struct Admission {
    snapshot: TaskRecord,
    events: Option<broadcast::Receiver<A2aEvent>>,
    deduped: bool,
}
impl Admission {
    fn retry(snapshot: TaskRecord) -> Self {
        Self {
            snapshot,
            events: None,
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
    fn owner(&self, params: &ServiceParams) -> Result<Principal, A2AError> {
        let owner = self.identity.resolve_service_params(params)?;
        if let Some(rules) = self.backend.store.access_rules() {
            if owner.user_id().is_none() {
                return Err(crate::identity::missing_identity());
            }
            if !rules.can_see_agent(
                &self.export.agent_ref(),
                &owner.user_id().into_iter().collect::<Vec<_>>(),
            ) {
                return Err(not_found());
            }
        }
        Ok(owner)
    }
    async fn session(
        &self,
        owner: &Principal,
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
    async fn task(&self, owner: &Principal, id: &str) -> Result<TaskRecord, A2AError> {
        self.backend
            .store
            .get_task_for_export(&self.export, owner, id)
            .await
            .map_err(map_error)?
            .ok_or_else(not_found)
    }
    async fn reconcile(
        &self,
        owner: &Principal,
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
    async fn wait_terminal(
        &self,
        owner: &Principal,
        mut record: TaskRecord,
    ) -> Result<TaskRecord, A2AError> {
        // KV is authoritative even if an event receiver lags or closes. Waiting
        // never owns the turn and dropping this future never sends cancellation.
        while !record.task.status.state.is_terminal() {
            tokio::time::sleep(Duration::from_millis(25)).await;
            record = self.task(owner, &record.task.id).await?;
        }
        Ok(record)
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
            self.wait_terminal(&owner, record).await?
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
        let records = self
            .backend
            .store
            .list_tasks(&self.export, &owner, context)
            .await
            .map_err(map_error)?
            .ok_or_else(not_found)?;
        let mut tasks = Vec::new();
        for record in records {
            let mut task = self.reconcile(&owner, record).await?.task;
            if !task_matches(&task, &req) {
                continue;
            }
            project_task(&mut task, &req)?;
            tasks.push(task);
        }
        paginate(tasks, &req)
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let owner = self.owner(params)?;
        let (admission, length) = self.admit_request(owner.clone(), req).await?;
        let (mut snapshot, events) = if admission.deduped {
            let record = self.reconcile(&owner, admission.snapshot).await?;
            // Dedupe retries stream the existing task, including terminal snapshots.
            self.backend
                .runner
                .stream_snapshot(&self.export, &owner, &record.task.id)
                .await
                .map_err(map_error)?
        } else {
            (admission.snapshot, admission.events)
        };
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
