//! Protocol methods log the final result, including failures before admission.

use super::errors::{map_error, not_found};
use super::{
    listing, return_immediately,
    task_view::{history, validate_history},
    HarnxHandler,
};
use crate::{request_log::RequestLog, runner::RunnerError};
use a2a_lf::{
    A2AError, AgentCard, CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
    GetExtendedAgentCardRequest, GetTaskPushNotificationConfigRequest, GetTaskRequest,
    ListTaskPushNotificationConfigsRequest, ListTaskPushNotificationConfigsResponse,
    ListTasksRequest, ListTasksResponse, SendMessageRequest, SendMessageResponse, StreamResponse,
    SubscribeToTaskRequest, Task, TaskPushNotificationConfig,
};
use a2a_server_lf::{handler::RequestHandler, middleware::ServiceParams};
use async_trait::async_trait;
use futures::stream::BoxStream;

#[async_trait]
impl RequestHandler for HarnxHandler {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        let log =
            RequestLog::new(&self.export.public_name, "SendMessage").with_message(&req.message);
        let result = async {
            let owner = self.owner(params).await?;
            let immediate = return_immediately(req.configuration.as_ref());
            let (admission, length) = self.admit_request(owner.clone(), req).await?;
            let mut task = self.resolve_send_task(&owner, admission, immediate).await?;
            history(&mut task, length)?;
            Ok(SendMessageResponse::Task(task))
        }
        .await;
        log.complete(result, RequestLog::send_response)
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        let log = RequestLog::new(&self.export.public_name, "GetTask").with_task(&req.id);
        let result = async {
            let owner = self.owner(params).await?;
            let mut task = self
                .reconcile(&owner, self.task(&owner, &req.id).await?)
                .await?
                .task;
            history(&mut task, req.history_length)?;
            Ok(task)
        }
        .await;
        log.complete(result, RequestLog::task)
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        let log = RequestLog::new(&self.export.public_name, "CancelTask").with_task(&req.id);
        let result = async {
            let owner = self.owner(params).await?;
            let initial = self.task(&owner, &req.id).await?;
            #[cfg(test)]
            if let Some(gate) = &self.cancel_snapshot_gate {
                gate.after_snapshot(initial.task.status.state.clone()).await;
            }
            if let Some(task) = self
                .cancel_hitl_if_active(&owner, &req.id, &initial)
                .await?
            {
                return Ok(task);
            }
            self.cancel_standard_task(&owner, &req.id, initial).await
        }
        .await;
        log.complete(result, RequestLog::task)
    }

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        let log = RequestLog::new(&self.export.public_name, "ListTasks")
            .with_context(req.context_id.as_deref());
        let result = async {
            let owner = self.owner(params).await?;
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
        .await;
        log.complete(result, RequestLog::list_response)
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let log = RequestLog::new(&self.export.public_name, "SendStreamingMessage")
            .with_message(&req.message)
            .streaming(self.backend.abort.clone());
        let result = async {
            let owner = self.owner(params).await?;
            let (admission, length) = self.admit_request(owner.clone(), req).await?;
            let (mut snapshot, events) = if admission.direct || admission.events.is_some() {
                (admission.snapshot, admission.events)
            } else {
                self.open_reconciled_stream(&owner, admission.snapshot)
                    .await?
            };
            history(&mut snapshot.task, length)?;
            Ok(crate::sse::task_stream(snapshot, events))
        }
        .await;
        log.stream(result)
    }

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let log = RequestLog::new(&self.export.public_name, "SubscribeToTask")
            .with_task(&req.id)
            .streaming(self.backend.abort.clone());
        let result = async {
            let owner = self.owner(params).await?;
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
        .await;
        log.stream(result)
    }

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        _req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let log = RequestLog::new(&self.export.public_name, "CreateTaskPushNotificationConfig")
            .with_task(&_req.task_id);
        let result = async {
            self.owner(params).await?;
            Err(A2AError::push_notification_not_supported())
        }
        .await;
        log.complete(result, |_, _| {})
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        _req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let log = RequestLog::new(&self.export.public_name, "GetTaskPushNotificationConfig")
            .with_task(&_req.task_id);
        let result = async {
            self.owner(params).await?;
            Err(A2AError::push_notification_not_supported())
        }
        .await;
        log.complete(result, |_, _| {})
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        _req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        let log = RequestLog::new(&self.export.public_name, "ListTaskPushNotificationConfigs")
            .with_task(&_req.task_id);
        let result = async {
            self.owner(params).await?;
            Err(A2AError::push_notification_not_supported())
        }
        .await;
        log.complete(result, |_, _| {})
    }

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        _req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        let log = RequestLog::new(&self.export.public_name, "DeleteTaskPushNotificationConfig")
            .with_task(&_req.task_id);
        let result = async {
            self.owner(params).await?;
            Err(A2AError::push_notification_not_supported())
        }
        .await;
        log.complete(result, |_, _| {})
    }

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        _req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        let log = RequestLog::new(&self.export.public_name, "GetExtendedAgentCard");
        let result = async {
            self.owner(params).await?;
            Err(A2AError::unsupported_operation(
                "extended agent cards are not supported",
            ))
        }
        .await;
        log.complete(result, |_, _| {})
    }
}
