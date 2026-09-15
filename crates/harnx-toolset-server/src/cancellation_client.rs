//! A transport timeout leaves acceptance Unknown, not physical cleanup Unconfirmed.
use harnx_toolset::{
    CancelAcceptance, CancellationAcknowledgement, ControlMessage, TOOL_PROTOCOL_VERSION,
};
use std::time::Duration;

pub async fn request_cancellation(
    client: &async_nats::Client,
    subject: String,
    control: &ControlMessage,
    timeout: Duration,
) -> CancellationAcknowledgement {
    let response = async {
        let payload = serde_json::to_vec(control)?;
        let response = client.request(subject, payload.into()).await?;
        let ack: CancellationAcknowledgement = serde_json::from_slice(&response.payload)?;
        anyhow::ensure!(
            ack.protocol_version == TOOL_PROTOCOL_VERSION
                && ack.generation == *control.execution.generation()
                && ack.operation_id == control.operation_id
                && ack.cancellation_id == control.cancellation_id,
            "cancellation acknowledgement identity mismatch"
        );
        Ok::<_, anyhow::Error>(ack)
    };
    match tokio::time::timeout(timeout, response).await {
        Ok(Ok(ack)) => ack,
        result => control.acknowledgement(
            CancelAcceptance::Unknown {
                reason: format!(
                    "cancellation acknowledgement unavailable; retry the same identity: {result:?}"
                ),
            },
            None,
        ),
    }
}
