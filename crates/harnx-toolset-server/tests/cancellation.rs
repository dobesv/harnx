mod common;
use anyhow::{Context, Result};
use common::{request_headers, wait_for_registration, TestHarness};
use futures_util::StreamExt;
use harnx_execution_control::{
    CleanupState, ExecutionContext, ExecutionStore, OperationRef, Owner,
};
use harnx_toolset::{
    CancelAcceptance, CancellationAcknowledgement, ControlMessage, ToolReply, ToolRequest,
};
use serde_json::json;
use std::time::Duration;

async fn setup() -> Result<(TestHarness, ExecutionStore, ExecutionContext)> {
    let harness = TestHarness::start()
        .await?
        .context("nats-server required")?;
    wait_for_registration(&harness.client, &harness.instance_id).await?;
    let store =
        ExecutionStore::ensure(&async_nats::jetstream::new(harness.client.clone()), 1).await?;
    let root = store.session("ack-parent", None, None).await?;
    store
        .claim(
            &root.reference,
            Owner {
                instance_id: "worker".into(),
                fence: 1,
            },
        )
        .await?;
    let root = store.activate_gate(&root.reference).await?;
    store
        .child(
            OperationRef::new("ack-parent", "ack-tool"),
            root.operation().clone(),
        )
        .await?;
    Ok((harness, store, root))
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
async fn v4_rejects_wrong_identity_and_recognizes_retired_finished_calls() -> Result<()> {
    let (harness, store, root) = setup().await?;
    let request = cancellation_request("echo");
    harness
        .client
        .request_with_headers(
            harness.echo_subject(),
            request_headers("ack-tool", "ack-tool"),
            serde_json::to_vec(&request)?.into(),
        )
        .await?;
    let producer = harness
        .toolset
        .last_context
        .lock()
        .await
        .as_ref()
        .unwrap()
        .execution
        .clone()
        .unwrap();
    wait_cleanup(&store, &producer, CleanupState::Confirmed).await?;
    store.status(root.operation()).await?;
    assert!(store.get(producer.operation()).await?.is_none());
    let mut control = ControlMessage::cancel(producer, "____test".into(), "finished-cancel".into());
    let request_ack = |control: ControlMessage| {
        let client = harness.client.clone();
        let subject = harness.instance_id.control_subject();
        async move {
            harnx_toolset_server::cancellation_client::request_cancellation(
                &client,
                subject,
                &control,
                Duration::from_secs(2),
            )
            .await
        }
    };
    assert_eq!(
        request_ack(control.clone()).await.acceptance,
        CancelAcceptance::AlreadyFinished
    );
    control.protocol_version = 3;
    assert!(matches!(
        request_ack(control).await.acceptance,
        CancelAcceptance::Rejected { .. }
    ));
    assert!(store
        .gate_stop(root.gate_root(), root.operation())
        .await?
        .is_none());
    Ok(())
}

async fn exercise_interruption(tool: &str, release: bool) -> Result<()> {
    let (harness, store, root) = setup().await?;
    let request = cancellation_request(tool);
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
    let producer = harness
        .toolset
        .last_context
        .lock()
        .await
        .as_ref()
        .unwrap()
        .execution
        .clone()
        .unwrap();
    let control = ControlMessage::cancel(producer.clone(), "____test".into(), "ack-cancel".into());
    let ack = tokio::time::timeout(
        Duration::from_secs(2),
        harness.client.request(
            harness.instance_id.control_subject(),
            serde_json::to_vec(&control)?.into(),
        ),
    )
    .await??;
    let ack: CancellationAcknowledgement = serde_json::from_slice(&ack.payload)?;
    assert!(matches!(ack.acceptance, CancelAcceptance::Accepted { .. }));
    assert_eq!(ack.protocol_version, 4);
    assert_eq!(ack.generation, *root.generation());
    assert_ne!(ack.cleanup.unwrap().state, CleanupState::Confirmed);
    let reply = tokio::time::timeout(Duration::from_secs(2), &mut invoke).await??;
    assert!(matches!(
        serde_json::from_slice::<ToolReply>(&reply.payload)?.result,
        Err(harnx_toolset::ToolErrorPayload::Interrupted(_))
    ));
    // No release signal has been sent. Neither a stubborn handler nor its
    // missing physical confirmation can keep the caller waiting for a reply.
    wait_cleanup(&store, &producer, CleanupState::Unconfirmed).await?;
    assert!(
        !store
            .get(producer.operation())
            .await?
            .unwrap()
            .owner_stopped
    );
    assert!(store
        .gate_stop(root.gate_root(), root.operation())
        .await?
        .is_none());
    if release {
        harness.toolset.allow_cleanup.notify_one();
        wait_cleanup(&store, &producer, CleanupState::Confirmed).await?;
    }
    Ok(())
}

async fn wait_cleanup(
    store: &ExecutionStore,
    producer: &ExecutionContext,
    state: CleanupState,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut watch = store.watch().await?;
        loop {
            if store.gate_cleanup(producer).await?.state == state {
                return Ok(());
            }
            watch.next().await.context("cleanup watch closed")??;
        }
    })
    .await?
}

fn cancellation_request(tool: &str) -> ToolRequest {
    ToolRequest {
        execution: None,
        replay_execution: None,
        replay: None,
        operation_id: "ack-tool".into(),
        call_id: "ack-tool".into(),
        tool: tool.into(),
        args: json!({"gate_cleanup": true}),
        parent_session_id: Some("ack-parent".into()),
        tool_call_id: None,
        capabilities: Default::default(),
    }
}
