use super::*;
use crate::execution_fence::GenerationFence;
use crate::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig};
use crate::nats_session_log::NatsSessionLog;
use harnx_execution_control::{ExecutionStore, Owner};
use tokio::sync::Barrier;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_error_reducer_does_not_cover_a_later_retry_prompt() -> Result<()> {
    let server = crate::nats_test_common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let lease = Arc::new(
        NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: js.clone(),
            session_id: "error-coverage",
            worker_id: "worker".into(),
            generation: 1,
            config: NatsLeaseConfig::default(),
            session_metadata: None,
        })
        .await?
        .context("lease")?,
    );
    let store = ExecutionStore::ensure(&js, 1).await?;
    let root = store.session("error-coverage", None, Some("g1")).await?;
    let owner = Owner {
        instance_id: lease.worker_id().into(),
        fence: lease.fence_token(),
    };
    store.claim(&root.reference, owner.clone()).await?;
    let ctx = store.activate_gate(&root.reference).await?;
    let backend = NatsSessionLogBackend::new(js.clone(), "error-coverage")
        .with_execution(Some(GenerationFence::new(store.clone(), ctx)));
    let ready = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let reducer = {
        let (ready, release, store, reference) = (
            ready.clone(),
            release.clone(),
            store.clone(),
            root.reference.clone(),
        );
        tokio::spawn(async move {
            let sequence = WorkerRuntime::record_session_error(
                &backend,
                &lease,
                &anyhow::anyhow!("budget reached"),
            )
            .await
            .context("Error sequence")?;
            ready.wait().await;
            release.wait().await;
            store
                .record_coverage(&reference, &owner, sequence, false)
                .await?;
            store.seal(&reference, &owner).await
        })
    };
    ready.wait().await;
    store.reserve_prompt(&root.reference, "retry").await?;
    let retry_sequence = NatsSessionLog::new(js, "error-coverage")
        .append_event_async(&retry_entry())
        .await?;
    store
        .commit_prompt(&root.reference, "retry", retry_sequence)
        .await?;
    release.wait().await;
    assert!(
        !reducer.await??,
        "Error cannot seal coverage of a later retry"
    );
    let operation = store.get(&root.reference).await?.unwrap();
    assert!(operation.covered_through < retry_sequence);
    Ok(())
}

fn retry_entry() -> harnx_core::session::SessionLogEntry {
    harnx_core::session::SessionLogEntry::Message {
        id: Some("retry".into()),
        role: harnx_core::message::MessageRole::User,
        content: harnx_core::message::MessageContent::Text("retry".into()),
        timestamp: None,
        fence_token: None,
    }
}
