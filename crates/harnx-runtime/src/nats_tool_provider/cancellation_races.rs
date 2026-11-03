//! Delay dispatch and control delivery independently to reproduce admission races.
use super::*;
use futures_util::StreamExt;
use harnx_core::abort::create_abort_signal;
use harnx_time_tools::TimeToolset;
use harnx_toolset::{CancelAcceptance, CancellationAcknowledgement, ToolInvokeError, Toolset};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct ObservedTimeToolset(mpsc::UnboundedSender<CancellationToken>);

#[async_trait]
impl Toolset for ObservedTimeToolset {
    fn name(&self) -> &str {
        "time"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        TimeToolset::new().tools()
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        let _ = self.0.send(cancel.clone());
        TimeToolset::new().invoke(tool, args, cancel).await
    }
}

struct Fixture {
    provider: NatsToolProvider,
    requests: async_nats::Subscriber,
    request_subject: String,
    controls: async_nats::Subscriber,
    remote_scope: ServerScope,
    started: mpsc::UnboundedReceiver<CancellationToken>,
    server_task: tokio::task::JoinHandle<anyhow::Result<()>>,
    _broker: crate::nats_test_common::NatsServerHandle,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server_task.abort();
    }
}

impl Fixture {
    async fn start() -> anyhow::Result<Self> {
        let broker = crate::nats_test_common::spawn_nats_server()
            .await?
            .context("nats-server required")?;
        let client = async_nats::connect(broker.url()).await?;
        let mut provider = super::tests::make_test_provider(&client).await;
        provider.parent_session_id = Some("cancel-race-session".into());
        let remote_scope = ServerScope::new();
        let (started_tx, started) = mpsc::unbounded_channel();
        let scope = remote_scope.clone();
        let url = broker.url().to_owned();
        let server_task = tokio::spawn(async move {
            harnx_toolset_server::serve_over_nats(ObservedTimeToolset(started_tx), scope, &url, "")
                .await
        });
        wait_for_registration(&client, &remote_scope).await?;
        let request_subject = client.new_inbox();
        let requests = client.subscribe(request_subject.clone()).await?;
        let controls = client
            .subscribe(provider.instance_id.control_subject())
            .await?;
        client.flush().await?;
        Ok(Self {
            provider,
            requests,
            request_subject,
            controls,
            remote_scope,
            started,
            server_task,
            _broker: broker,
        })
    }

    fn pending(&self) -> PendingToolRequest {
        let route = RegisteredTool {
            server: "____time".into(),
            selector_server: "time".into(),
            raw_name: "wait".into(),
            request_timeout: None,
        };
        let mut pending = self
            .provider
            .prepare_request(json!({"seconds": 300.0}), &route, None)
            .unwrap_or_else(|_| panic!("prepare request"));
        pending.subject = self.request_subject.clone();
        pending
    }

    async fn next_cancel(&mut self) -> anyhow::Result<ControlMessage> {
        let message = next_message(&mut self.controls).await?;
        Ok(serde_json::from_slice(&message.payload)?)
    }

    async fn forward_cancel(&self, control: &ControlMessage) -> anyhow::Result<CancelAcceptance> {
        let reply = self
            .provider
            .client
            .request(
                self.remote_scope.control_subject(),
                serde_json::to_vec(control)?.into(),
            )
            .await?;
        let acknowledgement: CancellationAcknowledgement = serde_json::from_slice(&reply.payload)?;
        assert_eq!(acknowledgement.cancellation_id, control.cancellation_id);
        Ok(acknowledgement.acceptance)
    }

    async fn forward_request(&self, message: async_nats::Message) -> anyhow::Result<()> {
        self.provider
            .client
            .publish_with_reply_and_headers(
                self.remote_scope.tool_subject("____time", "wait"),
                message.reply.context("request reply inbox")?,
                message.headers.context("request headers")?,
                message.payload,
            )
            .await?;
        self.provider.client.flush().await?;
        Ok(())
    }
}

async fn next_message(
    subscriber: &mut async_nats::Subscriber,
) -> anyhow::Result<async_nats::Message> {
    tokio::time::timeout(Duration::from_secs(5), subscriber.next())
        .await
        .context("message did not arrive")?
        .context("subscription closed")
}

async fn wait_for_registration(
    client: &async_nats::Client,
    scope: &ServerScope,
) -> anyhow::Result<()> {
    let js = async_nats::jetstream::new(client.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(store) = js.get_key_value(TOOL_REGISTRY_BUCKET).await {
                if store
                    .get(registration_key(scope, "____time"))
                    .await?
                    .is_some()
                {
                    return Ok::<_, anyhow::Error>(());
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

#[tokio::test]
async fn dropped_wait_retries_cancel_rejected_before_journaling() -> anyhow::Result<()> {
    assert_admission_race(false, false).await
}

#[tokio::test]
async fn aborted_wait_retries_cancel_rejected_before_journaling() -> anyhow::Result<()> {
    assert_admission_race(false, true).await
}

#[tokio::test]
async fn dropped_wait_retries_orphan_cancel_accepted_before_registration() -> anyhow::Result<()> {
    assert_admission_race(true, false).await
}

#[tokio::test]
async fn aborted_wait_retries_orphan_cancel_accepted_before_registration() -> anyhow::Result<()> {
    assert_admission_race(true, true).await
}

async fn assert_admission_race(journaled: bool, explicit_abort: bool) -> anyhow::Result<()> {
    let mut fixture = Fixture::start().await?;
    let pending = fixture.pending();
    let request = pending.durable.clone();
    if journaled {
        fixture
            .provider
            .journal()
            .await?
            .record(
                &request,
                ("wait", fixture.remote_scope.as_str(), "____time"),
            )
            .await?;
    }
    let abort = create_abort_signal();
    let mut wait = Box::pin(fixture.provider.await_response(pending, &abort));
    let message = tokio::select! {
        _ = &mut wait => panic!("wait finished before dispatch"),
        message = next_message(&mut fixture.requests) => message?,
    };
    if explicit_abort {
        abort.set_ctrlc();
        assert!(wait.await.is_err());
    } else {
        drop(wait);
        assert!(!abort.aborted());
    }
    let first = fixture.next_cancel().await?;
    // No live call exists yet. Exercise the real server's two orphan decisions.
    let acceptance = fixture.forward_cancel(&first).await?;
    match acceptance {
        CancelAcceptance::Accepted => assert!(journaled),
        CancelAcceptance::Rejected { reason } => {
            assert!(!journaled);
            assert_eq!(reason, "unknown call");
        }
        other => anyhow::bail!("unexpected early cancel response: {other:?}"),
    }
    fixture.forward_request(message).await?;
    let remote_cancel = tokio::time::timeout(Duration::from_secs(5), fixture.started.recv())
        .await?
        .context("remote invocation did not start")?;
    assert!(!remote_cancel.is_cancelled());
    for _ in 1..cancellation::CANCEL_ATTEMPTS {
        let retry = fixture.next_cancel().await?;
        assert_eq!(
            retry, first,
            "retry must retain the full cancellation identity"
        );
        fixture.forward_cancel(&retry).await?;
    }
    tokio::time::timeout(Duration::from_secs(5), remote_cancel.cancelled()).await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(300), fixture.controls.next())
            .await
            .is_err(),
        "cancel retries must stop after their fixed budget"
    );
    Ok(())
}

#[tokio::test]
async fn stale_wait_failure_preserves_same_id_replacement_without_cancelling() -> anyhow::Result<()>
{
    let mut fixture = Fixture::start().await?;
    let pending = fixture.pending();
    let call_id = pending.call_id.clone();
    let abort = create_abort_signal();
    let mut old_wait = Box::pin(fixture.provider.await_response(pending, &abort));
    tokio::select! {
        _ = &mut old_wait => panic!("wait finished before reservation"),
        message = next_message(&mut fixture.requests) => { message?; }
    }
    // insert replaces and drops the old sender, making supervised_failure ready.
    let (mut replacement_failure, replacement_identity) = fixture
        .provider
        .in_flight
        .register(InFlightRegistration {
            call_id: call_id.clone(),
            server: "____time".into(),
            session_id: "replacement-session".into(),
            control_subject: fixture.provider.instance_id.control_subject(),
        })
        .await;
    assert!(matches!(old_wait.await, Err(ToolError::Recoverable(_))));
    let snapshot = fixture
        .provider
        .in_flight
        .snapshot_for_session("replacement-session")
        .await;
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].call_id, call_id);
    assert!(matches!(
        replacement_failure.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(300), fixture.controls.next())
            .await
            .is_err()
    );
    assert!(fixture
        .provider
        .in_flight
        .complete(&call_id, &replacement_identity));
    Ok(())
}
