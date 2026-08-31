use anyhow::{Context, Result};
use harnx_core::instance::ServerScope;

pub(super) async fn subscribe_to_requests(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    identity_token: &str,
    readiness: Option<&harnx_healthz::Readiness>,
) -> Result<(async_nats::Subscriber, async_nats::Subscriber)> {
    let tool_subject = instance_id.tool_subject(identity_token, ">");
    let control_subject = instance_id.control_subject();
    let tool_requests = client
        .queue_subscribe(tool_subject.clone(), identity_token.to_owned())
        .await
        .with_context(|| format!("subscribe to tool requests on {tool_subject}"))?;
    let controls = client
        .subscribe(control_subject.clone())
        .await
        .with_context(|| format!("subscribe to controls on {control_subject}"))?;

    // Both subscriptions must be active before registration makes this server discoverable.
    client.flush().await.context("flush tool subscriptions")?;
    if let Some(readiness) = readiness {
        readiness.ready();
    }
    Ok((tool_requests, controls))
}
