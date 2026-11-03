//! Cancellation is addressed by session and call id, not by execution-control
//! generation. Cleanup is fire-and-forget, with a short bounded resend window
//! to cover cancellation racing ahead of remote invocation registration.
use super::*;

pub(super) const CANCEL_ATTEMPTS: usize = 3;
const CANCEL_RETRY_DELAY: Duration = Duration::from_millis(100);
const CANCEL_PUBLISH_TIMEOUT: Duration = Duration::from_secs(1);

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
    /// Resend the same cancellation three times to cover pre-registration
    /// races. Each publish is bounded; no attempt waits for an acknowledgement.
    pub(super) fn schedule_cancel(&self, request: &ToolRequest, server: &str) {
        let target = InFlightCancelTarget {
            call_id: request.call_id.clone(),
            server: server.to_owned(),
            control_subject: self.instance_id.control_subject(),
        };
        // Match InvocationExecution::claim for calls without a parent session.
        let session = request
            .parent_session_id
            .clone()
            .unwrap_or_else(|| request.call_id.clone());
        let client = self.client.clone();
        let cancellation_id = Uuid::now_v7().to_string();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            log::debug!("tool cancel not published: no current Tokio runtime");
            return;
        };
        runtime.spawn(async move {
            for attempt in 0..CANCEL_ATTEMPTS {
                if attempt > 0 {
                    tokio::time::sleep(CANCEL_RETRY_DELAY).await;
                }
                let result = tokio::time::timeout(
                    CANCEL_PUBLISH_TIMEOUT,
                    publish_tool_cancel(&client, &target, &session, &cancellation_id),
                )
                .await;
                if !matches!(&result, Ok(Ok(()))) {
                    log::debug!(
                        "tool cancel publish attempt {} failed: {result:?}",
                        attempt + 1
                    );
                }
            }
        });
    }
}
