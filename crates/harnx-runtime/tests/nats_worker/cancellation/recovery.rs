//! A replacement worker finishes what a dead owner left half-done.
use super::*;
use anyhow::Context;
use harnx_runtime::nats_lease::{NatsLeaseAcquireParams, NatsSessionLease};

/// The owner disappears mid-turn without releasing its lease. Once that lease
/// expires, the replacement picks the session up, sees the `Cancel` that
/// landed meanwhile, winds the interrupted turn up, and never calls the model.
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
        session_id: session.storage_key(),
        worker_id: "dead-owner".into(),
        generation: 1,
        config: lease_config.clone(),
        session_metadata: None,
    })
    .await?
    .context("acquire dead owner lease")?;
    let log =
        harnx_runtime::nats_session_log::NatsSessionLog::new(js.clone(), session.storage_key());
    seed_unanswered_round(&log, lease.fence_token()).await?;
    // Simulate a process disappearing before it writes Cancel or releases its
    // lease. Retain the test object so Drop cannot perform a graceful release.
    lease.stop_renewal_for_test().await;

    let outcome = session.interrupt("client cancel").await?;
    assert!(
        matches!(outcome, InterruptOutcome::Accepted { .. }),
        "the dead owner's turn must be interruptible, got {outcome:?}"
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let mut daemon_config = WorkerDaemonConfig::managing("local", "replacement");
    daemon_config.lease = lease_config;
    let daemon = tokio::spawn(run_worker_daemon(
        local_nats_runtime_config(server.url()),
        daemon_config,
        Some(counting_stub_call_fn(calls.clone())),
        None,
    ));

    await_wind_up(&log).await?;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    daemon.abort();
    let _ = daemon.await;
    drop(lease);
    Ok(())
}

/// The dead owner leaves an unanswered tool round behind: that is what the
/// replacement owes a result for once it takes the session over.
async fn seed_unanswered_round(
    log: &harnx_runtime::nats_session_log::NatsSessionLog,
    fence_token: u64,
) -> Result<()> {
    log.append_event_async(&Entry::ToolCalls {
        text: "started before the crash".into(),
        thought: None,
        calls: vec![ToolCall::new(
            "unanswered".into(),
            json!({}),
            Some("orphan-call".into()),
            None,
        )],
        timestamp: None,
        fence_token: Some(fence_token),
    })
    .await
    .map(drop)
}

/// The wind-up answering that round is the proof the replacement took over.
async fn await_wind_up(log: &harnx_runtime::nats_session_log::NatsSessionLog) -> Result<()> {
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            let entries = log.load_events_latest_async().await?;
            if entries.iter().any(|(_, entry)| {
                matches!(
                    entry,
                    Entry::ToolResults { results, .. }
                        if results.iter().any(|r| r.id.as_deref() == Some("orphan-call"))
                )
            }) {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await?
}
