use super::*;

pub(super) async fn handle_control(message: async_nats::Message, context: &ToolRequestContext) {
    let Ok(control) = serde_json::from_slice::<ControlMessage>(&message.payload) else {
        return;
    };
    let call_id = header_value(&message, HDR_CALL_ID).unwrap_or(control.call_id);
    let Some(mut call) = context.in_flight.lock().await.get(&call_id).cloned() else {
        return;
    };
    if call.reference.execution_id != control.operation_id {
        return;
    }
    match control.kind {
        ControlKind::Cancel => {
            call.cancel.cancel();
            if context
                .execution_store
                .cancel_operation(&call.reference, Some(&control.cancellation_id), false)
                .await
                .is_err()
            {
                return;
            }
            if call.stopped.wait_for(|stopped| *stopped).await.is_err() {
                return;
            }
            if let Some(reply) = message.reply {
                let ack = harnx_toolset::CancellationAcknowledgement {
                    operation_id: control.operation_id,
                    cancellation_id: control.cancellation_id,
                    stopped: true,
                };
                if let Ok(payload) = serde_json::to_vec(&ack) {
                    let _ = context.client.publish(reply, payload.into()).await;
                }
            }
        }
    }
}
