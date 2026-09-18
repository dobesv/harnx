//! Cancel one running hook invocation, addressed by session and call ID.
//!
//! Hooks keep no invocation journal: a call ID this server is not currently
//! running is simply `AlreadyFinished` (or was never issued here), since
//! there is nothing durable to consult once its future is gone.
use super::*;
use harnx_toolset::{
    CancelAcceptance, CancellationAcknowledgement, ControlMessage, TOOL_PROTOCOL_VERSION,
};

pub(super) async fn handle_control(
    message: async_nats::Message,
    server: &str,
    active: &HookRegistry,
    client: &async_nats::Client,
) {
    let Ok(control) = serde_json::from_slice::<ControlMessage>(&message.payload) else {
        return;
    };
    if control.server != server {
        return;
    }
    let acceptance = accept_cancel(&control, active).await;
    let acknowledgement = control.acknowledgement(acceptance);
    send_acknowledgement(client, message.reply, &acknowledgement).await;
}

/// Whether this server accepts the cancel: rejected outright on a bad
/// identity, otherwise resolved against whichever hook invocation (if any)
/// is still running under that call ID.
async fn accept_cancel(control: &ControlMessage, active: &HookRegistry) -> CancelAcceptance {
    if !valid_identity(control) {
        return CancelAcceptance::Rejected {
            reason: "cancellation identity or protocol mismatch".into(),
        };
    }
    let hook = active.lock().await.get(&control.call_id).cloned();
    match hook {
        Some(hook) if hook.session_id == control.session_id => {
            hook.set_cancellation_id(&control.cancellation_id).await;
            hook.cancel.cancel();
            CancelAcceptance::Accepted
        }
        Some(_) => CancelAcceptance::Rejected {
            reason: "call belongs to another session".into(),
        },
        None => CancelAcceptance::AlreadyFinished,
    }
}

/// Publish `acknowledgement` to the control message's reply subject, if it
/// has one — a control message with no reply subject wants no response.
async fn send_acknowledgement(
    client: &async_nats::Client,
    reply: Option<async_nats::Subject>,
    acknowledgement: &CancellationAcknowledgement,
) {
    let Some(reply) = reply else {
        return;
    };
    let Ok(payload) = serde_json::to_vec(acknowledgement) else {
        return;
    };
    let _ = client.publish(reply, payload.into()).await;
}

fn valid_identity(control: &ControlMessage) -> bool {
    control.protocol_version == TOOL_PROTOCOL_VERSION
        && valid_cancellation_id(&control.cancellation_id)
}

fn valid_cancellation_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= 256
}
