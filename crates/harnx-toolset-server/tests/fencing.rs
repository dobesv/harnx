mod common;
use anyhow::{Context, Result};
use common::{request_headers, TestHarness, TestToolset};
use harnx_toolset::{ControlMessage, ToolErrorPayload, ToolReply, ToolRequest};
use harnx_toolset_server::invocation_journal::InvocationJournal;
use serde_json::json;
use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::sync::Barrier;

struct Fixture {
    harness: TestHarness,
    journal: InvocationJournal,
    request: ToolRequest,
}

impl Fixture {
    async fn new(toolset: TestToolset) -> Result<Self> {
        Self::with_args(toolset, json!({"value": 42})).await
    }

    async fn with_args(toolset: TestToolset, args: serde_json::Value) -> Result<Self> {
        let harness = TestHarness::with_toolset(toolset)
            .await?
            .context("nats-server required")?;
        common::wait_for_registration(&harness.client, &harness.instance_id).await?;
        let js = async_nats::jetstream::new(harness.client.clone());
        let mut request = common::request("fenced-parent", "call-one");
        request.args = args;
        let journal = InvocationJournal::ensure(&js, 1).await?;
        journal
            .record(&request, ("test_echo", "scope", "____test"), 1)
            .await?;
        Ok(Self {
            harness,
            journal,
            request,
        })
    }

    fn send(&self) -> tokio::task::JoinHandle<Result<ToolReply>> {
        let client = self.harness.client.clone();
        let subject = self.harness.echo_subject();
        let request = self.request.clone();
        tokio::spawn(async move {
            let message = client
                .send_request(
                    subject,
                    async_nats::Request::new()
                        .headers(request_headers(&request.call_id, &request.call_id))
                        .payload(serde_json::to_vec(&request)?.into())
                        .timeout(Some(Duration::from_secs(15))),
                )
                .await?;
            Ok(serde_json::from_slice(&message.payload)?)
        })
    }

    /// Cancel the in-flight call over the control subject, as the session's
    /// owner would.
    async fn cancel(&self) -> Result<()> {
        let control = ControlMessage::cancel(
            "____test".into(),
            self.request.parent_session_id.clone().unwrap(),
            self.request.call_id.clone(),
            "fencing-cancel".into(),
        );
        let ack = harnx_toolset_server::cancellation_client::request_cancellation(
            &self.harness.client,
            self.harness.instance_id.control_subject(),
            &control,
            Duration::from_secs(5),
        )
        .await;
        anyhow::ensure!(
            ack.acceptance == harnx_toolset::CancelAcceptance::Accepted,
            "cancel was not accepted: {ack:?}"
        );
        Ok(())
    }

    /// A journal opened over a fresh connection, as a replacement server does.
    async fn restart_reader(&self) -> Result<InvocationJournal> {
        let client = async_nats::ConnectOptions::new()
            .token(common::TOKEN.into())
            .connect(&self.harness._server.url)
            .await?;
        InvocationJournal::ensure(&async_nats::jetstream::new(client), 1).await
    }
}

fn assert_interrupted(reply: ToolReply) {
    assert!(
        matches!(reply.result, Err(ToolErrorPayload::Interrupted(_))),
        "{reply:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_before_journal_completion_discards_late_success_and_duplicate() -> Result<()> {
    let ready = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let mut tool = TestToolset::default();
    tool.reply_barriers = Some((ready.clone(), release.clone()));
    let mut fixture = Fixture::new(tool).await?;
    let call = fixture.send();
    ready.wait().await;
    fixture.cancel().await?;
    // The handler finishes successfully only after the call was cancelled.
    release.wait().await;
    assert_interrupted(call.await??);

    let recorded = fixture
        .journal
        .get(&fixture.request)
        .await?
        .context("journal row")?
        .reply
        .context("the interrupted reply is the call's durable outcome")?;
    assert_interrupted(recorded);
    // A duplicate is answered from the journal instead of running the tool again.
    assert_interrupted(fixture.send().await??);
    assert_eq!(
        fixture
            .harness
            .toolset
            .echo_invocations
            .load(Ordering::SeqCst),
        1
    );
    fixture.harness.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_recovers_saved_reply() -> Result<()> {
    let mut fixture = Fixture::new(TestToolset::default()).await?;
    // Larger than a control-plane record; the whole reply lives in the row.
    let reply = ToolReply {
        call_id: fixture.request.call_id.clone(),
        result: Ok(json!({"text": "x".repeat(96 * 1024)})),
    };
    fixture
        .journal
        .complete(&fixture.request, reply.clone())
        .await?;
    assert_eq!(
        fixture
            .restart_reader()
            .await?
            .completed_reply(&fixture.request)
            .await?,
        Some(reply.clone())
    );
    // The server serves the saved reply rather than invoking the tool.
    assert_eq!(fixture.send().await??, reply);
    assert_eq!(
        fixture
            .harness
            .toolset
            .echo_invocations
            .load(Ordering::SeqCst),
        0
    );
    fixture.harness.shutdown().await;
    Ok(())
}

/// The mirror of the test above: the cancellation is observed only after the
/// handler has already returned, so the call succeeded and the caller is told
/// so. A reply that is delivered must be the reply that is recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_success_delivered_despite_a_late_cancel_is_still_journaled() -> Result<()> {
    let args = json!({"cancel_before_returning": true});
    let mut fixture = Fixture::with_args(TestToolset::default(), args.clone()).await?;
    let delivered = fixture.send().await??;
    assert_eq!(delivered.result.clone().ok(), Some(args));
    assert_eq!(
        fixture
            .journal
            .get(&fixture.request)
            .await?
            .context("journal row")?
            .reply,
        Some(delivered),
        "the reply the caller received is the call's durable outcome"
    );
    assert_eq!(
        fixture
            .harness
            .toolset
            .echo_invocations
            .load(Ordering::SeqCst),
        1
    );
    fixture.harness.shutdown().await;
    Ok(())
}
