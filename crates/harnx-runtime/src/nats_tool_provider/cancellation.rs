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

/// One cancellation of one call, resent under the same cancellation id.
struct CancelSend {
    client: async_nats::Client,
    target: InFlightCancelTarget,
    session: String,
    cancellation_id: String,
}

impl CancelSend {
    /// Publish and flush once, bounded; no attempt waits for an acknowledgement.
    async fn attempt(&self, attempt: usize) {
        let result = tokio::time::timeout(
            CANCEL_PUBLISH_TIMEOUT,
            publish_tool_cancel(
                &self.client,
                &self.target,
                &self.session,
                &self.cancellation_id,
            ),
        )
        .await;
        if !matches!(&result, Ok(Ok(()))) {
            log::debug!(
                "tool cancel publish attempt {} failed: {result:?}",
                attempt + 1
            );
        }
    }

    /// Send every attempt from a detached task, so dropping the caller can't
    /// stop it. The receiver resolves once the first attempt has finished.
    fn spawn(self) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            log::debug!("tool cancel not published: no current Tokio runtime");
            return None;
        };
        let (first_sent, first) = tokio::sync::oneshot::channel();
        runtime.spawn(async move {
            let mut first_sent = Some(first_sent);
            for attempt in 0..CANCEL_ATTEMPTS {
                if attempt > 0 {
                    tokio::time::sleep(CANCEL_RETRY_DELAY).await;
                }
                self.attempt(attempt).await;
                if let Some(sent) = first_sent.take() {
                    let _ = sent.send(());
                }
            }
        });
        Some(first)
    }
}

impl NatsToolProvider {
    fn cancel_send(&self, request: &ToolRequest, server: &str) -> CancelSend {
        CancelSend {
            client: self.client.clone(),
            target: InFlightCancelTarget {
                call_id: request.call_id.clone(),
                server: server.to_owned(),
                control_subject: self.instance_id.control_subject(),
            },
            // Match InvocationExecution::claim for calls without a parent session.
            session: request
                .parent_session_id
                .clone()
                .unwrap_or_else(|| request.call_id.clone()),
            cancellation_id: Uuid::now_v7().to_string(),
        }
    }

    /// Ask the tool server to cancel `request`, without waiting for it.
    /// Resend the same cancellation three times to cover pre-registration
    /// races. For a call future dropped mid-flight, which can't await.
    pub(super) fn schedule_cancel(&self, request: &ToolRequest, server: &str) {
        self.cancel_send(request, server).spawn();
    }

    /// Like `schedule_cancel`, but returns only once the first publish has
    /// been flushed (or its bounded attempt failed). A detached task dies with
    /// the runtime, so a process that exits as soon as its calls return, such
    /// as harnx-mcp-server at stdio EOF, could otherwise lose the cancellation
    /// before the task first ran.
    pub(super) async fn send_cancel(&self, request: &ToolRequest, server: &str) {
        if let Some(first) = self.cancel_send(request, server).spawn() {
            let _ = first.await;
        }
    }
}
