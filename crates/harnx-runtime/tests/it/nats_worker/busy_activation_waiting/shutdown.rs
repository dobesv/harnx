use super::*;

/// Assert a consumer hands a delivery back right away: a shutdown NAK must not
/// leave a settled activation waiting out its ack wait.
async fn assert_immediately_deliverable(
    consumer: &async_nats::jetstream::consumer::PullConsumer,
) -> Result<()> {
    let mut batch = consumer
        .fetch()
        .max_messages(1)
        .expires(Duration::from_secs(2))
        .messages()
        .await?;
    let fetched = tokio::time::timeout(Duration::from_secs(3), batch.next()).await?;
    let fetched = fetched
        .transpose()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    assert!(
        fetched.is_some(),
        "a shutdown-settled delivery must be immediately deliverable"
    );
    Ok(())
}

/// Start a probe-and-fault worker whose shutdown is externally cancellable,
/// for tests that drive shutdown timing. Returns the cancellation token and
/// the daemon handle. The short activation ack wait bounds JetStream recovery
/// for deliveries an aborted waiter never settled.
async fn spawn_shutdown_probe_worker(
    server_url: &str,
    spec: FaultWorkerSpec,
    shutdown: CancellationToken,
) -> Result<(CancellationToken, AbortOnDropHandle<Result<()>>)> {
    use harnx_runtime::nats_worker::{run_worker_daemon_with_shutdown, DaemonRunOptions};
    let FaultWorkerSpec {
        worker_id,
        call_fn,
        probe,
        faults,
        per_session_capacity: _,
    } = spec;
    let mut daemon = WorkerDaemonConfig::managing("local", worker_id)
        .with_busy_waiter_probe_for_test(probe)
        .with_activation_faults_for_test(faults)
        .with_activation_ack_wait_for_test(Duration::from_secs(3));
    daemon.lease = fast_daemon_lease();
    let readiness = harnx_healthz::Readiness::default();
    let handle = AbortOnDropHandle::new(tokio::spawn(run_worker_daemon_with_shutdown(
        local_nats_runtime_config(server_url),
        daemon,
        DaemonRunOptions {
            call_fn: Some(call_fn),
            readiness: Some(readiness.clone()),
            shutdown: shutdown.clone(),
            shutdown_timeout: harnx_runtime::nats_worker::WORKER_SHUTDOWN_TIMEOUT,
        },
    )));
    wait_until(CI_SAFE_TIMEOUT, move || readiness.is_ready()).await?;
    Ok((shutdown, handle))
}

/// Fetch a delivery a settled (or aborted) waiter handed back. The dead
/// worker's own pull request releases the delivery once its closing
/// connection reaches the broker, so one long-lived fetch request observes
/// the recovery without racing it.
async fn fetch_handed_back_delivery(
    consumer: &async_nats::jetstream::consumer::PullConsumer,
) -> Result<()> {
    let mut batch = consumer
        .fetch()
        .max_messages(1)
        .expires(Duration::from_secs(15))
        .messages()
        .await?;
    let fetched = tokio::time::timeout(Duration::from_secs(20), batch.next()).await?;
    let fetched = fetched
        .transpose()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    assert!(
        fetched.is_some(),
        "the handed-back delivery must come back for redelivery"
    );
    Ok(())
}

/// Shutdown settles parked deliveries: each is NAKed back un-delayed, so
/// another worker can pick it up immediately and nothing is terminated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_settles_parked_busy_delivery() -> Result<()> {
    use harnx_runtime::nats_worker::{run_worker_daemon_with_shutdown, DaemonRunOptions};
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "busy-park-shutdown";
    let running_prompt = seed_admitted_user(&jetstream, session_id, "running turn").await?;
    let (gate, call_fn) = TurnGate::new();
    let mut daemon = WorkerDaemonConfig::managing("local", "busy-park-shutdown-worker");
    daemon.lease = fast_daemon_lease();
    let readiness = harnx_healthz::Readiness::default();
    let shutdown = CancellationToken::new();
    let daemon_handle = AbortOnDropHandle::new(tokio::spawn(run_worker_daemon_with_shutdown(
        local_nats_runtime_config(server.url()),
        daemon,
        DaemonRunOptions {
            call_fn: Some(call_fn),
            readiness: Some(readiness.clone()),
            shutdown: shutdown.clone(),
            shutdown_timeout: harnx_runtime::nats_worker::WORKER_SHUTDOWN_TIMEOUT,
        },
    )));
    wait_until(CI_SAFE_TIMEOUT, move || readiness.is_ready()).await?;
    publish_session_activate(
        &jetstream,
        "local",
        &SessionActivate::new(storage_key(session_id)).with_requested_seq(running_prompt),
        1,
    )
    .await?;
    gate.wait_until_calls(1).await?;
    publish_activation_for(&jetstream, session_id, "parked turn").await?;
    let consumer = worker_consumer(&jetstream).await?;
    wait_delivered_sequence(&consumer, 2, CI_SAFE_TIMEOUT).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    shutdown.cancel();
    let stopped = tokio::time::timeout(CI_SAFE_TIMEOUT, daemon_handle).await??;
    assert!(stopped.is_ok(), "worker shutdown must complete cleanly");

    let stream = jetstream.get_stream("WORK_NOTIFY_local").await?;
    assert_eq!(
        stream.get_info().await?.state.messages,
        2,
        "neither activation may be terminated by shutdown"
    );
    let info = consumer.get_info().await?;
    assert_eq!(info.num_ack_pending, 2, "both deliveries were handed back");
    // A shutdown NAK makes the parked activation deliverable right away; an
    // un-NAKed, un-heartbeated delivery would sit until its ack wait.
    assert_immediately_deliverable(&consumer).await?;
    Ok(())
}

/// An embedding caller that aborts the daemon future without cancelling its
/// shutdown token must not leave parked waiters behind: cancellation is tied
/// to the daemon future's own lifetime, so the waiter observes shutdown,
/// hands its delivery back un-delayed, and deregisters.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aborting_the_daemon_future_settles_parked_waiters() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "busy-park-abort";
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(BusyWaiterProbe::default());
    let faults = Arc::new(ActivationFaults::default());
    let worker = spawn_fault_probe_worker(
        server.url(),
        FaultWorkerSpec {
            worker_id: "busy-park-abort-worker",
            call_fn: counting_stub_call_fn(Arc::clone(&calls)),
            probe: Arc::clone(&probe),
            faults: Arc::clone(&faults),
            per_session_capacity: None,
        },
    )
    .await?;

    let foreign_lease =
        publish_foreign_busy_activation(&jetstream, session_id, "foreign-holder").await?;
    let consumer = worker_consumer(&jetstream).await?;
    wait_delivered_sequence(&consumer, 1, CI_SAFE_TIMEOUT).await?;
    wait_until_parked(&probe, 1).await?;

    // Abort the daemon future directly: the test never cancels a shutdown
    // token, so only the daemon's own drop-guard cancellation can settle the
    // parked waiter.
    worker.abort();
    let _ = worker.await;
    wait_until_registry_empty(&probe).await?;
    let stream = jetstream.get_stream("WORK_NOTIFY_local").await?;
    assert_eq!(
        stream.get_info().await?.state.messages,
        1,
        "the parked activation was handed back, not terminated"
    );
    foreign_lease.release().await?;
    Ok(())
}

/// A waiter stalled past the shutdown settle timeout must be aborted and
/// joined, not detached, and shutdown must reconcile the slot the aborted
/// waiter can never clear: the registry reports zero and the delivery was
/// handed back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_timeout_aborts_and_reconciles_a_stalled_waiter() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "busy-settle-timeout";
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(BusyWaiterProbe::default());
    let faults = Arc::new(ActivationFaults::default());
    // The waiter NAKs its delivery back, then stalls forever in the shutdown
    // flush, past the two-second settle budget.
    faults.stall_shutdown_flush.store(true, Ordering::SeqCst);
    let (shutdown, daemon_handle) = spawn_shutdown_probe_worker(
        server.url(),
        FaultWorkerSpec {
            worker_id: "busy-settle-timeout-worker",
            call_fn: counting_stub_call_fn(Arc::clone(&calls)),
            probe: Arc::clone(&probe),
            faults: Arc::clone(&faults),
            per_session_capacity: None,
        },
        CancellationToken::new(),
    )
    .await?;

    let foreign_lease =
        publish_foreign_busy_activation(&jetstream, session_id, "foreign-holder").await?;
    let consumer = worker_consumer(&jetstream).await?;
    wait_delivered_sequence(&consumer, 1, CI_SAFE_TIMEOUT).await?;
    wait_until_parked(&probe, 1).await?;
    // Let the waiter's immediate post-park claim finish, so shutdown lands
    // while it sleeps and the stall (not a mid-claim cancellation) drives
    // the settle timeout.
    tokio::time::sleep(Duration::from_millis(300)).await;

    shutdown.cancel();
    // The stalled waiter never reaches its own finish: only the settle
    // timeout's abort, and shutdown's reconciliation, can empty the registry.
    let stopped = tokio::time::timeout(CI_SAFE_TIMEOUT, daemon_handle).await??;
    assert!(stopped.is_ok(), "worker shutdown must complete cleanly");
    assert_eq!(
        probe.slots.load(Ordering::SeqCst),
        0,
        "the aborted waiter's slot must be reconciled"
    );
    assert_eq!(
        probe.parked.load(Ordering::SeqCst),
        0,
        "the parked count must be reconciled"
    );
    let stream = jetstream.get_stream("WORK_NOTIFY_local").await?;
    assert_eq!(
        stream.get_info().await?.state.messages,
        1,
        "the parked activation was handed back, not terminated"
    );
    // The waiter NAKed its delivery back before stalling, so the abort cannot
    // strand it.
    fetch_handed_back_delivery(&consumer).await?;
    foreign_lease.release().await?;
    Ok(())
}
