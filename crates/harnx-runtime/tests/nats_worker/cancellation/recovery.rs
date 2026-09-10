use super::*;
use anyhow::Context;
use harnx_execution_control::Owner;
use harnx_runtime::nats_lease::{NatsLeaseAcquireParams, NatsSessionLease};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replacement_confirms_durable_cancel_after_dead_owners_lease_expires() -> Result<()> {
    let server = require_nats_server()
        .await?
        .context("nats-server required")?;
    let session = session(server.url(), "dead-cancelling-owner").await?;
    session.enqueue_text("never run the model").await?;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let lease_config = NatsLeaseConfig {
        ttl: Duration::from_secs(1),
        renew_interval: Duration::from_millis(250),
        replicas: 1,
        ..Default::default()
    };
    let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream: js.clone(),
        session_id: session.session_id(),
        worker_id: "dead-owner".into(),
        generation: 1,
        config: lease_config.clone(),
        session_metadata: None,
    })
    .await?
    .context("acquire dead owner lease")?;
    let operation = session
        .execution_store()
        .current(session.session_id())
        .await?
        .unwrap();
    let old_fence = lease.fence_token();
    session
        .execution_store()
        .claim(
            &operation.reference,
            Owner {
                instance_id: "dead-owner".into(),
                fence: old_fence,
            },
        )
        .await?;
    // Simulate a process disappearing before it writes Cancel or releases its
    // lease. Retain the test object so Drop cannot perform a graceful release.
    lease.stop_renewal_for_test().await;
    let receipt = session.request_cancel(CancelRequest::default()).await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut daemon_config = WorkerDaemonConfig::managing("local", "replacement");
    daemon_config.lease = lease_config;
    let daemon = tokio::spawn(run_worker_daemon(
        local_nats_runtime_config(server.url()),
        daemon_config,
        Some(counting_stub_call_fn(calls.clone())),
        None,
    ));
    let status = session
        .wait_for_cancel(&receipt, tokio::time::Instant::now() + CI_SAFE_TIMEOUT)
        .await?;
    assert_eq!(status.disposition, CancelDisposition::Cancelled);
    assert_eq!(status.cancellation_id, receipt.cancellation_id);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let recovered = session
        .execution_store()
        .current(session.session_id())
        .await?
        .unwrap();
    assert!(recovered.owner.unwrap().fence > old_fence);
    let entries = NatsSessionLog::new(js, session.session_id())
        .load_events_async()
        .await?;
    assert!(entries.iter().any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { fence_token } if *fence_token > old_fence)));
    daemon.abort();
    let _ = daemon.await;
    drop(lease);
    Ok(())
}
