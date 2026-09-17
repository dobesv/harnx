//! Cancellation is addressed by session and call id, not by execution-control
//! generation. Publishing it is fire-and-forget cleanup work: the tool
//! server's journal, not a client-side retry loop, is what makes a cancel
//! eventually reach an orphaned call.
use super::*;

/// Publish one cancel control message to the call's owning instance.
pub async fn publish_tool_cancel(
    client: &async_nats::Client,
    target: &InFlightCancelTarget,
    session_id: &str,
    cancellation_id: &str,
) -> anyhow::Result<()> {
    let control = ControlMessage::cancel(
        target.server.clone(),
        session_id.to_string(),
        target.call_id.clone(),
        cancellation_id.to_string(),
    );
    client
        .publish(
            target.control_subject.clone(),
            serde_json::to_vec(&control)?.into(),
        )
        .await?;
    client.flush().await?;
    Ok(())
}

impl NatsToolProvider {
    /// Ask the tool server to cancel `request`, without waiting for it.
    /// Never blocks the caller and never fails the in-progress call: a lost
    /// cancel is retried by whoever asks again with a fresh cancellation id.
    pub(super) fn schedule_cancel(&self, request: &ToolRequest, server: &str) {
        let target = InFlightCancelTarget {
            call_id: request.call_id.clone(),
            server: server.to_owned(),
            control_subject: self.instance_id.control_subject(),
        };
        let session = request.parent_session_id.clone().unwrap_or_default();
        let client = self.client.clone();
        let cancellation_id = Uuid::now_v7().to_string();
        tokio::spawn(async move {
            if let Err(error) =
                publish_tool_cancel(&client, &target, &session, &cancellation_id).await
            {
                log::debug!("tool cancel not published: {error:#}");
            }
        });
    }
}
