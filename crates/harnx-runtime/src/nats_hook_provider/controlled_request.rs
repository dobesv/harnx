use super::*;
use crate::nats_tool_provider::{InFlightRegistration, NatsInFlightCalls};
use harnx_hookset_server::{decode_hook_reply, hook_request_headers};

pub(super) async fn request(
    client: &async_nats::Client,
    subject: String,
    payload: Vec<u8>,
    options: HookRequestOptions,
) -> Result<HookOutcome> {
    let HookRequestOptions {
        timeout,
        abort,
        instance_id,
        server,
    } = options;
    ensure_local(&abort)?;
    let value: serde_json::Value = serde_json::from_slice(&payload)?;
    let session_id = value
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .context("hook payload missing session_id")?
        .to_string();
    let call_id = uuid::Uuid::now_v7().to_string();

    let headers = hook_request_headers(&session_id, &call_id);

    // Registered in the same process-wide registry the tool provider uses
    // (keyed by instance scope), so a session-scoped cancel finds this call
    // alongside any in-flight tool calls while it waits on the reply below.
    // Checked before registering, not after, so an abort here never leaves an
    // orphaned entry between register and complete.
    ensure_local(&abort)?;
    let in_flight = NatsInFlightCalls::for_instance(&instance_id);
    let control_subject = instance_id.hook_control_subject(&server);
    let _failure = in_flight
        .register(InFlightRegistration {
            call_id: call_id.clone(),
            server,
            session_id,
            control_subject,
        })
        .await;

    let request = async_nats::Request::new()
        .payload(payload.into())
        .headers(headers)
        .timeout(None);
    let response = tokio::time::timeout(
        timeout,
        harnx_nats_common::rpc::request(client, subject, request),
    )
    .await;
    in_flight.complete(&call_id).await;
    match response {
        Ok(Ok(message)) => decode_hook_reply(&message.payload),
        error => anyhow::bail!("hook request did not acknowledge completion: {error:?}"),
    }
}

fn ensure_local(abort: &Option<harnx_core::abort::AbortSignal>) -> Result<()> {
    anyhow::ensure!(
        !abort.as_ref().is_some_and(|abort| abort.aborted()),
        "hook invocation interrupted"
    );
    Ok(())
}
