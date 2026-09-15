//! Execution snapshots update only their original generation's rows and views.
use crate::types::{SubAgentStatus, TranscriptItem, Tui};
use harnx_execution_control::{Operation, OperationState};

impl Tui {
    pub(crate) fn hydrate_execution_state(&mut self, cluster: String, operation: Operation) {
        if operation.stop_decision.is_some() {
            self.live_events.stop(&operation.reference.execution_id);
        }
        let root = self.session_activity_target.as_ref()
            == Some(&(operation.reference.session_id.clone(), cluster.clone()));
        if root
            && !self
                .live_events
                .matches(Some(&operation.reference.execution_id))
        {
            return;
        }
        let status = operation_status(&operation);
        update_cancelled_rows(&mut self.app.transcript, &cluster, &operation, &status);
        self.hydrate_child_execution(&cluster, &operation, &status);
        if root {
            self.resume_observed_cancellation(cluster, operation);
        }
    }

    fn resume_observed_cancellation(&mut self, cluster: String, operation: Operation) {
        if operation.stop_decision.is_some() {
            self.settle_interrupted_prompt();
            self.cancellation = None;
            self.pending_exit_cancel = None;
            return;
        }
        if operation.state.cancelling() && self.cancellation.is_none() {
            // Re-issue only after the attachment has loaded the same generation.
            // An old snapshot cannot start a cancellation against a replacement.
            self.start_observed_cancellation(
                operation.reference.session_id.clone(),
                cluster,
                operation.reference.execution_id.clone(),
            );
        }
    }

    fn hydrate_child_execution(
        &mut self,
        cluster: &str,
        operation: &Operation,
        status: &SubAgentStatus,
    ) {
        for (key, state) in &mut self.app.monitored_sessions {
            if same_session(key, operation, cluster)
                && state
                    .live_events
                    .matches(Some(&operation.reference.execution_id))
            {
                state.execution_id = Some(operation.reference.execution_id.clone());
                state.status = status.clone();
            }
            update_cancelled_rows(&mut state.transcript, cluster, operation, status);
        }
        self.hydrate_child_views(cluster, operation, status);
    }
    fn hydrate_child_views(
        &mut self,
        cluster: &str,
        operation: &Operation,
        status: &SubAgentStatus,
    ) {
        for view in &mut self.app.subagent_view_stack {
            if view.key.matches_operation(cluster, &operation.reference)
                && view.progress.as_ref().is_some_and(|progress| {
                    progress.snapshot.invocation_id == operation.reference.execution_id
                })
            {
                view.status = status.clone();
            }
        }
    }
}

fn same_session(
    key: &crate::types::MonitoredSessionKey,
    operation: &Operation,
    cluster: &str,
) -> bool {
    key.matches_operation(cluster, &operation.reference)
}

fn operation_status(operation: &Operation) -> SubAgentStatus {
    if operation.stop_decision.is_some() {
        return SubAgentStatus::Cancelled;
    }
    match operation.state {
        OperationState::CancelRequested | OperationState::Quiescing => SubAgentStatus::Cancelling,
        OperationState::Unconfirmed => SubAgentStatus::Unconfirmed,
        OperationState::Cancelled => SubAgentStatus::Cancelled,
        OperationState::Completed => SubAgentStatus::Completed,
        _ => SubAgentStatus::Running,
    }
}

fn update_cancelled_rows(
    items: &mut [TranscriptItem],
    cluster: &str,
    operation: &Operation,
    status: &SubAgentStatus,
) {
    if !operation.state.cancelling() && operation.state != OperationState::Cancelled {
        return;
    }
    for item in items {
        let TranscriptItem::SubAgentSession {
            key,
            invocation_id,
            status: row_status,
            ..
        } = item
        else {
            continue;
        };
        if key.matches_operation(cluster, &operation.reference)
            && invocation_id.as_deref() == Some(&operation.reference.execution_id)
        {
            *row_status = status.clone();
        }
    }
}
