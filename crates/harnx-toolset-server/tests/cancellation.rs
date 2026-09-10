mod common;
use anyhow::{Context, Result};
use common::{request_headers, wait_for_registration, TestHarness};
use harnx_toolset::{ControlKind, ControlMessage, ToolReply, ToolRequest};
use serde_json::json;
use std::time::Duration;
#[tokio::test(flavor = "multi_thread")]
async fn cancellation_ack_waits_for_cleanup_and_stubborn_calls_remain_unconfirmed() -> Result<()> {
    use harnx_execution_control::{ExecutionStore, OperationRef, OperationState, Owner};
    let harness = TestHarness::start()
        .await?
        .context("nats-server required")?;
    wait_for_registration(&harness.client, &harness.instance_id).await?;
    let js = async_nats::jetstream::new(harness.client.clone());
    let store = ExecutionStore::ensure(&js, 1).await?;
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
    let reference = OperationRef::new("ack-parent", "ack-tool");
    store
        .child(reference.clone(), root.reference.clone())
        .await?;
    let request = ToolRequest {
        operation_id: "ack-tool".into(),
        call_id: "ack-tool".into(),
        tool: "slow".into(),
        args: json!({"gate_cleanup": true}),
        parent_session_id: Some("ack-parent".into()),
        tool_call_id: None,
        capabilities: Default::default(),
    };
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
    let control = ControlMessage {
        call_id: "ack-tool".into(),
        operation_id: "ack-tool".into(),
        cancellation_id: "ack-cancel".into(),
        kind: ControlKind::Cancel,
    };
    let ack = harness.client.request_with_headers(
        harness.instance_id.control_subject(),
        request_headers("ack-tool", "ack-cancel"),
        serde_json::to_vec(&control)?.into(),
    );
    tokio::pin!(ack);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut ack)
            .await
            .is_err(),
        "ack preceded cleanup"
    );
    tokio::time::timeout(
        Duration::from_secs(2),
        harness.toolset.slow_cancelled.notified(),
    )
    .await?;
    store
        .mutate(&reference, |operation| {
            operation.cancellation.as_mut().unwrap().progress_at =
                operation.created_at - Duration::from_secs(6);
            Ok(())
        })
        .await?;
    assert_eq!(
        store.status(&reference).await?.state,
        OperationState::Unconfirmed
    );
    assert!(
        store
            .get(&root.reference)
            .await?
            .unwrap()
            .state
            .accepts_work(),
        "direct child cancellation must not cancel the parent"
    );
    harness.toolset.allow_cleanup.notify_one();
    let (ack, reply) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(&mut ack, &mut invoke)
    })
    .await?;
    let ack: harnx_toolset::CancellationAcknowledgement = serde_json::from_slice(&ack?.payload)?;
    assert!(ack.stopped);
    assert_eq!(ack.cancellation_id, "ack-cancel");
    assert_eq!(
        store.get(&reference).await?.unwrap().state,
        OperationState::Cancelled
    );
    assert!(serde_json::from_slice::<ToolReply>(&reply?.payload)?
        .result
        .is_err());
    Ok(())
}
