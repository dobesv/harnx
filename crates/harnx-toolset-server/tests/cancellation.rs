mod common;
use anyhow::{Context, Result};
use common::{request_headers, wait_for_registration, TestHarness};
use harnx_toolset::{
    CancelAcceptance, CancellationAcknowledgement, ControlMessage, ToolReply, TOOL_PROTOCOL_VERSION,
};
use harnx_toolset_server::cancellation_client::request_cancellation;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Generous enough that a contended CI runner cannot fail a condition poll
/// that a healthy one settles in milliseconds.
const CI_SAFE_TIMEOUT: Duration = Duration::from_secs(60);

async fn setup() -> Result<TestHarness> {
    let harness = TestHarness::start()
        .await?
        .context("nats-server required")?;
    wait_for_registration(&harness.client, &harness.instance_id).await?;
    Ok(harness)
}

fn cancel(cancellation_id: &str) -> ControlMessage {
    ControlMessage::cancel(
        "____test".into(),
        "ack-parent".into(),
        "ack-tool".into(),
        cancellation_id.into(),
    )
}

async fn acknowledgement(
    harness: &TestHarness,
    control: ControlMessage,
) -> CancellationAcknowledgement {
    request_cancellation(
        &harness.client,
        harness.instance_id.control_subject(),
        &control,
        Duration::from_secs(2),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn acceptance_precedes_cleanup_and_later_confirmation_is_independent() -> Result<()> {
    exercise_interruption("slow", true).await
}

#[tokio::test(flavor = "multi_thread")]
async fn never_finishing_handler_cannot_delay_acceptance_or_interrupted_reply() -> Result<()> {
    exercise_interruption("never", false).await
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_wrong_identity_and_recognizes_finished_calls() -> Result<()> {
    let harness = setup().await?;
    let mut control = cancel("wrong-protocol");
    control.protocol_version = 4;
    assert!(matches!(
        acknowledgement(&harness, control).await.acceptance,
        CancelAcceptance::Rejected { .. }
    ));
    let mut control = cancel("blank-cancellation");
    control.cancellation_id = "   ".into();
    assert!(matches!(
        acknowledgement(&harness, control).await.acceptance,
        CancelAcceptance::Rejected { .. }
    ));

    // A call that already replied is no longer in-flight, but its journal row
    // still says so: the orphan path must recognize it as finished rather
    // than treat it as unknown.
    let request = common::request("finished-parent", "finished-tool");
    let reply = harness
        .client
        .send_request(
            harness.echo_subject(),
            async_nats::Request::new()
                .headers(request_headers("finished-tool", "finished-tool"))
                .payload(serde_json::to_vec(&request)?.into())
                .timeout(None),
        )
        .await?;
    let reply: ToolReply = serde_json::from_slice(&reply.payload)?;
    assert!(reply.result.is_ok());
    let control = ControlMessage::cancel(
        "____test".into(),
        "finished-parent".into(),
        "finished-tool".into(),
        "finished-cancel".into(),
    );
    assert_eq!(
        acknowledgement(&harness, control).await.acceptance,
        CancelAcceptance::AlreadyFinished
    );
    Ok(())
}

/// The server's side of a caller restart: a cancel arrives for a call this
/// process never had in flight, so the journal row is all there is to act on.
/// The checkpoint the original invocation left behind is what reaches the
/// toolset, which is how a resumable tool is stopped without the process that
/// started it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphan_cancel_delivers_checkpoint_to_toolset_after_caller_restart() {
    // A toolset whose `cancel` records what it received.
    type Seen = Arc<Mutex<Option<(String, Option<serde_json::Value>)>>>;
    struct Remote {
        seen: Seen,
    }
    #[async_trait::async_trait]
    impl harnx_toolset::Toolset for Remote {
        fn name(&self) -> &str {
            "remote"
        }
        fn tools(&self) -> Vec<harnx_toolset::ToolSpec> {
            vec![common::tool_spec("start")]
        }
        async fn invoke(
            &self,
            _tool: &str,
            _args: serde_json::Value,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<serde_json::Value, harnx_toolset::ToolInvokeError> {
            unreachable!()
        }
        async fn cancel(
            &self,
            invocation: harnx_toolset::ToolInvocation,
        ) -> Result<(), harnx_toolset::ToolInvokeError> {
            *self.seen.lock().await = Some((
                invocation.context.call_id.clone(),
                invocation.context.checkpoint.clone(),
            ));
            Ok(())
        }
    }
    let seen = Arc::new(Mutex::new(None));
    let (server, client, journal) = common::serve(Remote { seen: seen.clone() }).await;
    let request = common::request("sess-1", "call-9");
    journal
        .record(&request, ("start", "scope", server.identity()), 1)
        .await
        .unwrap();
    journal
        .checkpoint(
            "sess-1",
            "call-9",
            serde_json::json!({"remote_session": "r-1"}),
        )
        .await
        .unwrap();
    let ack = harnx_toolset_server::cancellation_client::request_cancellation(
        &client,
        server.control_subject(),
        &harnx_toolset::ControlMessage::cancel(
            server.identity().into(),
            "sess-1".into(),
            "call-9".into(),
            "c-1".into(),
        ),
        std::time::Duration::from_secs(2),
    )
    .await;
    assert!(matches!(
        ack.acceptance,
        harnx_toolset::CancelAcceptance::Accepted
    ));
    let (call_id, checkpoint) = seen.lock().await.clone().expect("toolset cancel invoked");
    assert_eq!(call_id, "call-9");
    assert_eq!(checkpoint.unwrap()["remote_session"], "r-1");
}

/// A cancel whose acknowledgement never reached the caller is re-sent under
/// the same `cancellation_id`. The server has to treat the repeat as the same
/// cancellation it already applied — never a second one, and never a rejection
/// — so the caller can keep asking until it hears an answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_cancel_ack_is_recovered_by_cancellation_id() -> Result<()> {
    const CANCELLATION: &str = "lost-ack";
    let harness = setup().await?;
    let mut request = common::request("ack-parent", "ack-tool");
    request.tool = "slow".into();
    request.args = serde_json::json!({"park_cleanup": true});
    let invoke = harness.client.send_request(
        harness.instance_id.tool_subject("____test", "slow"),
        async_nats::Request::new()
            .headers(request_headers("ack-tool", "ack-tool"))
            .payload(serde_json::to_vec(&request)?.into())
            .timeout(None),
    );
    tokio::pin!(invoke);
    tokio::select! {
        _ = harness.toolset.slow_started.notified() => {},
        result = &mut invoke => anyhow::bail!("invocation finished early: {result:?}"),
    }

    // The cancel lands, but with no reply subject its acknowledgement goes
    // nowhere: from the caller's side it is indistinguishable from a lost ack.
    harness
        .client
        .publish(
            harness.instance_id.control_subject(),
            serde_json::to_vec(&cancel(CANCELLATION))?.into(),
        )
        .await?;
    harness.client.flush().await?;

    let reply = tokio::time::timeout(CI_SAFE_TIMEOUT, &mut invoke).await??;
    let reply: ToolReply = serde_json::from_slice(&reply.payload)?;
    let Err(harnx_toolset::ToolErrorPayload::Interrupted(interrupted)) = reply.result else {
        anyhow::bail!("expected an interrupted reply, got {reply:?}");
    };
    assert_eq!(
        interrupted.cancellation_id.as_deref(),
        Some(CANCELLATION),
        "the cancel that was never acknowledged is still the one that stopped the call"
    );

    // The handler is still parked in its cleanup, so the call is still this
    // process's: re-sending the same id repeats the acceptance it never sent.
    assert_eq!(
        acknowledgement(&harness, cancel(CANCELLATION))
            .await
            .acceptance,
        CancelAcceptance::Accepted
    );

    harness.toolset.allow_cleanup.notify_one();
    tokio::time::timeout(CI_SAFE_TIMEOUT, harness.toolset.slow_finished.notified())
        .await
        .context("cancelled handler never finished after its cleanup was released")?;

    // Once the handler is gone the same id is answered from the journal row
    // the interrupted reply wrote. Polled, because the call leaves the
    // in-flight map a moment after the handler itself returns.
    let deadline = std::time::Instant::now() + CI_SAFE_TIMEOUT;
    loop {
        let acceptance = acknowledgement(&harness, cancel(CANCELLATION))
            .await
            .acceptance;
        if acceptance == CancelAcceptance::AlreadyFinished {
            break;
        }
        assert_eq!(
            acceptance,
            CancelAcceptance::Accepted,
            "a repeat of a known cancellation is never rejected or unknown"
        );
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the finished call never settled as AlreadyFinished"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(())
}

async fn exercise_interruption(tool: &str, release: bool) -> Result<()> {
    let harness = setup().await?;
    let mut request = common::request("ack-parent", "ack-tool");
    request.tool = tool.into();
    request.args = serde_json::json!({"park_cleanup": true});
    let invoke = harness.client.send_request(
        harness.instance_id.tool_subject("____test", tool),
        async_nats::Request::new()
            .headers(request_headers("ack-tool", "ack-tool"))
            .payload(serde_json::to_vec(&request)?.into())
            .timeout(None),
    );
    tokio::pin!(invoke);
    tokio::select! {
        _ = harness.toolset.slow_started.notified() => {},
        result = &mut invoke => anyhow::bail!("invocation finished early: {result:?}"),
    }
    let ack = acknowledgement(&harness, cancel("ack-cancel")).await;
    assert_eq!(ack.acceptance, CancelAcceptance::Accepted);
    assert_eq!(ack.protocol_version, TOOL_PROTOCOL_VERSION);
    assert_eq!(ack.session_id, "ack-parent");
    assert_eq!(ack.call_id, "ack-tool");
    // No release signal has been sent: neither a stubborn handler nor one that
    // never finishes can keep the caller waiting for its reply.
    let reply = tokio::time::timeout(Duration::from_secs(2), &mut invoke).await??;
    let reply: ToolReply = serde_json::from_slice(&reply.payload)?;
    let Err(harnx_toolset::ToolErrorPayload::Interrupted(interrupted)) = reply.result else {
        anyhow::bail!("expected an interrupted reply, got {reply:?}");
    };
    assert_eq!(interrupted.cancellation_id.as_deref(), Some("ack-cancel"));
    if release {
        // Acceptance and the reply both landed while the handler was still
        // parked in its cleanup. Releasing it lets that finish afterwards, on
        // its own schedule: the server kept polling the handler rather than
        // dropping it when it answered the caller.
        harness.toolset.allow_cleanup.notify_one();
        tokio::time::timeout(
            Duration::from_secs(5),
            harness.toolset.slow_finished.notified(),
        )
        .await
        .context("cancelled handler never finished after its cleanup was released")?;
    }
    Ok(())
}
