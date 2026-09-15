use super::*;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use crate::nats_test_common as common;

#[tokio::test]
async fn cancellation_before_first_poll_never_starts_cooperative_work_for_cleanup(
) -> anyhow::Result<()> {
    use anyhow::Context;
    let nats = common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let client = async_nats::connect(nats.url()).await?;
    let store = ExecutionStore::ensure(&async_nats::jetstream::new(client), 1).await?;
    let root = store.session("pre-start", None, Some("g1")).await?;
    store
        .claim(&root.reference, Owner::invocation("worker"))
        .await?;
    let root = store.activate_gate(&root.reference).await?;
    let tool = store
        .child(
            OperationRef::new("pre-start", "tool"),
            root.operation().clone(),
        )
        .await?;
    store
        .claim(&tool.reference, Owner::invocation("tool-server"))
        .await?;
    let producer = store.activate_gate(&tool.reference).await?;
    let execution = InvocationExecution {
        store: store.clone(),
        reference: tool.reference,
        producer: producer.clone(),
        watch: store.watch().await?,
    };
    let cancel = CancellationToken::new();
    cancel.cancel();
    let started = Arc::new(AtomicBool::new(false));
    let work = {
        let started = started.clone();
        async move {
            started.store(true, Ordering::SeqCst);
            Ok(Value::Null)
        }
    };
    let (reply, result) = tokio::sync::oneshot::channel();
    execution
        .invoke(cancel, CancellationGuarantee::Cooperative, work, reply)
        .await;
    assert!(matches!(
        result.await?,
        Err(ToolInvokeError::Interrupted(_))
    ));
    assert!(!started.load(Ordering::SeqCst));
    assert_eq!(
        store.gate_cleanup(&producer).await?.state,
        harnx_execution_control::CleanupState::Confirmed
    );
    Ok(())
}
