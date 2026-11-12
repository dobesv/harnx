//! The new-message control: it ends an opted-in wait early without cancelling
//! the call, so the handler's own reply is what the caller receives.
use crate::common;
use anyhow::{Context, Result};
use common::request_headers;
use harnx_toolset::{
    CancelAcceptance, ControlMessage, ToolErrorPayload, ToolInvocation, ToolInvokeError, ToolReply,
    ToolRequest, ToolSpec, Toolset,
};
use harnx_toolset_server::cancellation_client::request_cancellation;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const CI_SAFE_TIMEOUT: Duration = Duration::from_secs(60);

/// `wait` returns its own result when a new message arrives; `stubborn`
/// ignores the signal and ends only when cancelled. Neither has an orphan
/// `cancel`, so a signal that reached it would panic the server task.
#[derive(Clone, Default)]
struct Waiting {
    started: Arc<Notify>,
}

#[async_trait::async_trait]
impl Toolset for Waiting {
    fn name(&self) -> &str {
        "waiting"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![
            common::tool_spec("wait").with_stop_on_new_message(),
            common::tool_spec("stubborn").with_stop_on_new_message(),
        ]
    }

    async fn invoke(
        &self,
        _tool: &str,
        _args: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        unreachable!("the server always invokes with context")
    }

    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        self.started.notify_one();
        let new_message = invocation.context.new_message.clone();
        let stops_early = invocation.tool == "wait";
        tokio::select! {
            _ = new_message.cancelled(), if stops_early => {
                Ok(json!({"status": "running", "stopped_by": "new_message"}))
            }
            _ = invocation.cancel.cancelled() => Err(ToolInvokeError::Fatal("cancelled".into())),
        }
    }

    async fn cancel(&self, _invocation: ToolInvocation) -> Result<(), ToolInvokeError> {
        panic!("a new-message signal must never reach the orphan cancel path")
    }
}

fn request(tool: &str, call_id: &str) -> ToolRequest {
    ToolRequest {
        tool: tool.into(),
        ..common::request("new-message-session", call_id)
    }
}

struct Call {
    server: common::ServedToolset,
    client: async_nats::Client,
    journal: harnx_toolset_server::invocation_journal::InvocationJournal,
    toolset: Waiting,
}

impl Call {
    async fn serve() -> Self {
        let toolset = Waiting::default();
        let (server, client, journal) = common::serve(toolset.clone()).await;
        Self {
            server,
            client,
            journal,
            toolset,
        }
    }

    /// Start `tool` as call `call_id` and return its pending reply once the
    /// handler is running.
    async fn start(
        &self,
        tool: &str,
        call_id: &str,
    ) -> Result<tokio::task::JoinHandle<Result<ToolReply>>> {
        self.start_request(request(tool, call_id)).await
    }

    async fn start_request(
        &self,
        request: ToolRequest,
    ) -> Result<tokio::task::JoinHandle<Result<ToolReply>>> {
        let started = self.toolset.started.notified();
        let reply = tokio::spawn({
            let client = self.client.clone();
            let subject = self.server.tool_subject(&request.tool);
            let payload = serde_json::to_vec(&request)?;
            let headers = request_headers(&request.call_id, &request.call_id);
            async move {
                let message = client
                    .send_request(
                        subject,
                        async_nats::Request::new()
                            .headers(headers)
                            .payload(payload.into())
                            .timeout(None),
                    )
                    .await?;
                Ok(serde_json::from_slice(&message.payload)?)
            }
        });
        tokio::time::timeout(CI_SAFE_TIMEOUT, started)
            .await
            .context("tool handler never started")?;
        Ok(reply)
    }

    async fn send(&self, control: ControlMessage) -> CancelAcceptance {
        request_cancellation(
            &self.client,
            self.server.control_subject(),
            &control,
            Duration::from_secs(2),
        )
        .await
        .acceptance
    }

    fn new_message(&self, call_id: &str) -> ControlMessage {
        ControlMessage::new_message(
            self.server.identity().into(),
            "new-message-session".into(),
            call_id.into(),
        )
    }
}

fn stopped_reply() -> Result<Value, ToolErrorPayload> {
    Ok(json!({"status": "running", "stopped_by": "new_message"}))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_message_control_returns_the_handlers_own_reply() -> Result<()> {
    let call = Call::serve().await;
    let mut reply = call.start("wait", "wait-call").await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut reply)
            .await
            .is_err(),
        "without a new message the wait keeps waiting"
    );
    assert_eq!(
        call.send(call.new_message("wait-call")).await,
        CancelAcceptance::Accepted
    );
    let reply = tokio::time::timeout(CI_SAFE_TIMEOUT, reply).await???;
    assert_eq!(reply.result, stopped_reply());
    Ok(())
}

/// A message already waiting when the call was dispatched ends the wait at
/// once, with no control message to race the call's admission.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_message_pending_at_dispatch_returns_the_wait_at_once() -> Result<()> {
    let call = Call::serve().await;
    let mut pending = request("wait", "pending-call");
    pending
        .capabilities
        .insert(harnx_toolset::CAPABILITY_NEW_MESSAGE_PENDING.into());
    let reply = call.start_request(pending).await?;
    let reply = tokio::time::timeout(CI_SAFE_TIMEOUT, reply).await???;
    assert_eq!(reply.result, stopped_reply());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_message_control_leaves_cancel_able_to_interrupt() -> Result<()> {
    let call = Call::serve().await;
    let mut reply = call.start("stubborn", "stubborn-call").await?;
    assert_eq!(
        call.send(call.new_message("stubborn-call")).await,
        CancelAcceptance::Accepted
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut reply)
            .await
            .is_err(),
        "a new message must not end a call that ignores it"
    );
    let cancel = ControlMessage::cancel(
        call.server.identity().into(),
        "new-message-session".into(),
        "stubborn-call".into(),
        "after-new-message".into(),
    );
    assert_eq!(call.send(cancel).await, CancelAcceptance::Accepted);
    let reply = tokio::time::timeout(CI_SAFE_TIMEOUT, reply).await???;
    let Err(ToolErrorPayload::Interrupted(interrupted)) = reply.result else {
        anyhow::bail!("expected an interrupted reply, got {reply:?}");
    };
    assert_eq!(
        interrupted.cancellation_id.as_deref(),
        Some("after-new-message")
    );
    Ok(())
}

/// A journaled call this process is not running is what a cancel would hand
/// to the toolset's orphan `cancel`; a new message must stop short of that,
/// and since the call may simply not be admitted yet, ask for a retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_message_control_for_a_call_not_running_here_asks_for_a_retry() -> Result<()> {
    let call = Call::serve().await;
    let orphaned = request("wait", "orphaned-call");
    call.journal
        .record(&orphaned, ("wait", "scope", call.server.identity()))
        .await?;
    // The server's own answer, not the client's timeout fallback, which is
    // also `Unknown` and is what a panicking orphan `cancel` would produce.
    let acceptance = call.send(call.new_message("orphaned-call")).await;
    assert!(
        matches!(&acceptance, CancelAcceptance::Unknown { reason } if reason.contains("not running here")),
        "{acceptance:?}"
    );
    Ok(())
}
