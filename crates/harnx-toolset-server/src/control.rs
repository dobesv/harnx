use super::*;
use harnx_execution_control::{CleanupStatus, ExecutionContext, InterruptScope, LogicalState};
use harnx_toolset::{CancelAcceptance, CancellationAcknowledgement};

pub(super) async fn handle_control(message: async_nats::Message, context: &ToolRequestContext) {
    let Ok(control) = serde_json::from_slice::<ControlMessage>(&message.payload) else {
        return;
    };
    if control.server != context.server_identity {
        return;
    }
    let call = context
        .in_flight
        .lock()
        .await
        .get(&control.call_id)
        .cloned();
    let acknowledgement = if !valid_identity(&message, &control, call.as_ref()) {
        control.acknowledgement(
            CancelAcceptance::Rejected {
                reason: "cancellation identity or protocol mismatch".into(),
            },
            None,
        )
    } else {
        // Fence the local capability before any broker I/O. Closing a reply does
        // not undo an external side effect, and remote work is not an ack prerequisite.
        if let Some(call) = &call {
            call.cancel.cancel();
        }
        match tokio::time::timeout(
            Duration::from_secs(2),
            accept(context, &control, call.as_ref()),
        )
        .await
        {
            Ok(Ok(ack)) => ack,
            result => control.acknowledgement(
                CancelAcceptance::Unknown {
                    reason: format!(
                        "stop acceptance unknown; retry this cancellation ID: {result:?}"
                    ),
                },
                None,
            ),
        }
    };
    if let Some(reply) = message.reply {
        if let Ok(payload) = serde_json::to_vec(&acknowledgement) {
            let _ = context.client.publish(reply, payload.into()).await;
        }
    }
}

fn valid_identity(
    message: &async_nats::Message,
    control: &ControlMessage,
    call: Option<&execution::ActiveCall>,
) -> bool {
    control.protocol_version == TOOL_PROTOCOL_VERSION
        && valid_request_ids(control)
        && control.operation_id == control.call_id
        && control.execution.operation().execution_id == control.operation_id
        && header_value(message, HDR_CALL_ID).is_none_or(|header| header == control.call_id)
        && call.is_none_or(|call| same_execution(&call.producer, &control.execution))
}

fn valid_request_ids(control: &ControlMessage) -> bool {
    let id = &control.cancellation_id;
    if id.trim().is_empty() || id.len() > 256 {
        return false;
    }
    [
        control.execution.operation(),
        control.execution.generation(),
        control.execution.gate_root(),
    ]
    .iter()
    .all(|reference| reference.validate().is_ok())
}

fn same_execution(left: &ExecutionContext, right: &ExecutionContext) -> bool {
    left.operation() == right.operation()
        && left.generation() == right.generation()
        && left.gate_root() == right.gate_root()
}

async fn accept(
    context: &ToolRequestContext,
    control: &ControlMessage,
    call: Option<&execution::ActiveCall>,
) -> Result<CancellationAcknowledgement> {
    let store = &context.execution_store;
    let producer = store
        .gate_context(control.execution.gate_root(), control.execution.operation())
        .await?;
    anyhow::ensure!(
        same_execution(&producer, &control.execution),
        "cancellation generation changed"
    );
    let stop = if let Some(stop) = store
        .gate_stop(producer.gate_root(), producer.operation())
        .await?
    {
        stop
    } else {
        if store.gate_logical_state(&producer).await? == LogicalState::Completed {
            return Ok(control.acknowledgement(CancelAcceptance::AlreadyFinished, None));
        }
        match store
            .interrupt(
                &InterruptScope {
                    gate_root: producer.gate_root().clone(),
                    operation: producer.operation().clone(),
                    reason: "tool control cancellation".into(),
                },
                &control.cancellation_id,
            )
            .await
        {
            Ok(stop) => stop,
            Err(error) => {
                if store.gate_logical_state(&producer).await? == LogicalState::Completed {
                    return Ok(control.acknowledgement(CancelAcceptance::AlreadyFinished, None));
                }
                return Err(error);
            }
        }
    };
    // No broker read after the durable receipt: an optional cleanup snapshot
    // timing out must not downgrade known acceptance to Unknown.
    let cleanup = CleanupStatus::default();
    // The receipt is authoritative now. Bookkeeping and owner discovery run in
    // the supervisor, including a restart with no local handle for this operation.
    let store = store.clone();
    let missing_owner = call.is_none();
    context.cleanup.spawn(async move {
        let _ = store
            .cancel_operation(producer.operation(), None, false)
            .await;
        if missing_owner && cleanup.state != harnx_execution_control::CleanupState::Confirmed {
            let _ = store
                .record_cleanup(
                    &producer,
                    CleanupStatus::unconfirmed(
                        "resource owner handle unavailable; shutdown not confirmed",
                    ),
                )
                .await;
        }
    });
    Ok(control.acknowledgement(CancelAcceptance::Accepted { stop }, Some(cleanup)))
}
