//! Cancel one tool call, addressed by session and call ID.
//!
//! A call this process is still running is stopped locally. A call this
//! process is not running (its owner is gone, or it never ran here) is
//! answered from the invocation journal instead: unrecorded calls are
//! rejected, calls that already replied are `AlreadyFinished`, and everything
//! else is hand off to the toolset's own `cancel` with whatever checkpoint the
//! original invocation left behind.
use super::*;
use harnx_toolset::CancelAcceptance;

pub(super) async fn handle_control(message: async_nats::Message, context: &ToolRequestContext) {
    let Ok(control) = serde_json::from_slice::<ControlMessage>(&message.payload) else {
        return;
    };
    if control.server != context.server_identity {
        return;
    }
    let acknowledgement = control.acknowledgement(decide(context, &control).await);
    let Some(reply) = message.reply else {
        return;
    };
    let Ok(payload) = serde_json::to_vec(&acknowledgement) else {
        return;
    };
    let _ = context.client.publish(reply, payload.into()).await;
}

/// Answer the cancel: a call this process is running is stopped locally, a
/// call it is not running is decided from the journal instead.
async fn decide(context: &ToolRequestContext, control: &ControlMessage) -> CancelAcceptance {
    if !valid_identity(control) {
        return CancelAcceptance::Rejected {
            reason: "cancellation identity or protocol mismatch".into(),
        };
    }
    let active = context
        .in_flight
        .lock()
        .await
        .get(&control.call_id)
        .cloned();
    match active {
        Some(call) if call.session_id == control.session_id => stop_call(&call, control),
        Some(_) => CancelAcceptance::Rejected {
            reason: "call belongs to another session".into(),
        },
        None => orphan_cancel(context, control).await,
    }
}

/// Fence the local capability before answering. Closing a reply does not undo
/// an external side effect, but the caller is owed a decision now — and that
/// decision outranks any result the handler still manages to produce.
fn stop_call(call: &execution::ActiveCall, control: &ControlMessage) -> CancelAcceptance {
    call.set_cancellation_id(&control.cancellation_id);
    call.cancel.cancel();
    CancelAcceptance::Accepted
}

/// Answer a cancel for a call with no local `ActiveCall`, from its journal
/// row alone.
async fn orphan_cancel(context: &ToolRequestContext, control: &ControlMessage) -> CancelAcceptance {
    let record = match context
        .journal
        .recorded(&control.session_id, &control.call_id)
        .await
    {
        Ok(Some(record)) => record,
        Ok(None) => {
            return CancelAcceptance::Rejected {
                reason: "unknown call".into(),
            }
        }
        Err(error) => {
            return CancelAcceptance::Unknown {
                reason: format!("journal read failed: {error:#}"),
            }
        }
    };
    if record.reply.is_some() {
        return CancelAcceptance::AlreadyFinished;
    }
    let invocation =
        crate::invocation::orphan_invocation(context, &record.request, record.checkpoint.clone());
    match tokio::time::timeout(Duration::from_secs(2), context.toolset.cancel(invocation)).await {
        Ok(Ok(())) => CancelAcceptance::Accepted,
        Ok(Err(error)) => CancelAcceptance::Rejected {
            reason: format!("{error}"),
        },
        Err(_) => CancelAcceptance::Unknown {
            reason: "toolset cancel timed out; retry".into(),
        },
    }
}

fn valid_identity(control: &ControlMessage) -> bool {
    control.protocol_version == TOOL_PROTOCOL_VERSION
        && valid_cancellation_id(&control.cancellation_id)
}

fn valid_cancellation_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= 256
}
