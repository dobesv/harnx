use crate::{Operation, OperationKind, OperationState};

pub(crate) fn transition(previous: OperationState, operation: &Operation) {
    if previous == operation.state {
        return;
    }
    let kind = match operation.kind {
        OperationKind::Session => "session",
        OperationKind::Tool => "tool",
    };
    metrics::counter!("harnx_execution_transitions_total", "kind" => kind, "state" => state_name(operation.state)).increment(1);
    if operation.state == OperationState::Cancelled {
        if let Some(cancel) = &operation.cancellation {
            metrics::histogram!("harnx_cancellation_quiescence_seconds", "kind" => kind).record(
                (chrono::Utc::now() - cancel.requested_at)
                    .num_milliseconds()
                    .max(0) as f64
                    / 1000.0,
            );
        }
    }
}

fn state_name(state: OperationState) -> &'static str {
    match state {
        OperationState::Preparing => "preparing",
        OperationState::Running => "running",
        OperationState::CancelRequested => "cancel_requested",
        OperationState::Quiescing => "quiescing",
        OperationState::Unconfirmed => "unconfirmed",
        OperationState::Completed => "completed",
        OperationState::Cancelled => "cancelled",
    }
}
