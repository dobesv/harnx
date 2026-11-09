use super::*;

/// A claim error at wake time must settle the parked delivery exactly once:
/// a delayed claim-error NAK, heartbeat stopped, waiter deregistered — and
/// the delivery claims cleanly once the fault clears.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_waiter_wake_claim_error_hands_delivery_back_and_deregisters() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(BusyWaiterProbe::default());
    let faults = Arc::new(ActivationFaults::default());
    let worker = spawn_fault_probe_worker(
        server.url(),
        FaultWorkerSpec {
            worker_id: "busy-wake-error-worker",
            call_fn: counting_stub_call_fn(Arc::clone(&calls)),
            probe: Arc::clone(&probe),
            faults: Arc::clone(&faults),
            per_session_capacity: None,
        },
    )
    .await?;

    let session_id = "busy-wake-claim-error";
    let prompt = seed_admitted_user(&jetstream, session_id, "wake into an error").await?;
    let foreign_lease =
        acquire_worker_lease(&jetstream, &storage_key(session_id), "foreign-holder").await?;
    publish_session_activate(
        &jetstream,
        "local",
        &SessionActivate::new(storage_key(session_id)).with_requested_seq(prompt),
        1,
    )
    .await?;
    let consumer = worker_consumer(&jetstream).await?;
    wait_delivered_sequence(&consumer, 1, CI_SAFE_TIMEOUT).await?;
    wait_until_parked(&probe, 1).await?;

    // The parked waiter's wake claim fails: it must hand the delivery back
    // (delayed claim-error NAK, well under the busy-NAK delay) and leave the
    // registry.
    faults.fail_claims.store(true, Ordering::SeqCst);
    foreign_lease.release().await?;
    wait_until_registry_empty(&probe).await?;
    // The waiter hands the delivery back with a claim-error NAK (backoff
    // capped at 2s), so it redelivers and cycles while the fault is set.
    // CI_SAFE_TIMEOUT, not a tight bound: under stress-test load the whole
    // claim/NAK/redelivery cycle can take seconds of wall-clock, and the
    // fault-clearing phase below is what proves the delivery was handed
    // back exactly once.
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            if consumer.get_info().await?.num_redelivered >= 1 {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;

    // The next claim through the normal path succeeds: the delivery is
    // handed back once, then claimed and settled.
    faults.fail_claims.store(false, Ordering::SeqCst);
    {
        let calls = Arc::clone(&calls);
        wait_until(CI_SAFE_TIMEOUT, move || calls.load(Ordering::SeqCst) >= 1).await?;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    wait_until_nothing_pending(&consumer).await?;
    drop(worker);
    Ok(())
}

/// A delivery whose heartbeat already stopped (a wind-up lease lost
/// mid-claim) must never be parked: it is handed straight back, so no slot is
/// registered and no turn launches.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dead_heartbeat_delivery_is_handed_back_instead_of_parked() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "busy-dead-heartbeat";
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(BusyWaiterProbe::default());
    let faults = Arc::new(ActivationFaults::default());
    faults
        .busy_claims_dead_heartbeat
        .store(true, Ordering::SeqCst);
    let worker = spawn_fault_probe_worker(
        server.url(),
        FaultWorkerSpec {
            worker_id: "busy-dead-heartbeat-worker",
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
    // The claim returned busy with a dead heartbeat: the park gate must have
    // handed the delivery back instead of parking it.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        probe.parked.load(Ordering::SeqCst),
        0,
        "a dead-heartbeat delivery must not park"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no turn may launch");
    // The handback is a busy NAK, so the delivery redelivers within the busy
    // delay; a parked delivery would still be held here.
    wait_delivered_sequence(&consumer, 2, Duration::from_secs(25)).await?;
    assert_eq!(probe.parked.load(Ordering::SeqCst), 0);
    foreign_lease.release().await?;
    drop(worker);
    Ok(())
}

/// A waiter whose wake claim returns busy with a dead heartbeat must hand the
/// delivery back and deregister instead of re-sleeping on a delivery
/// JetStream would redeliver behind its back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dead_heartbeat_wake_hands_the_parked_delivery_back() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "busy-dead-wake";
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(BusyWaiterProbe::default());
    let faults = Arc::new(ActivationFaults::default());
    let worker = spawn_fault_probe_worker(
        server.url(),
        FaultWorkerSpec {
            worker_id: "busy-dead-wake-worker",
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

    // Every later claim returns busy with a dead heartbeat. The next wake is
    // the one-second recheck timer (the foreign lease stays held, and renewal
    // Puts do not wake), so the waiter must hand the delivery back well
    // inside CI_SAFE_TIMEOUT instead of re-sleeping.
    faults
        .busy_claims_dead_heartbeat
        .store(true, Ordering::SeqCst);
    wait_until_registry_empty(&probe).await?;
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no turn may launch");
    let stream = jetstream.get_stream("WORK_NOTIFY_local").await?;
    assert_eq!(
        stream.get_info().await?.state.messages,
        1,
        "the parked activation was handed back, not terminated"
    );
    // The handback is a busy NAK: the delivery redelivers within the busy
    // delay instead of staying parked.
    wait_delivered_sequence(&consumer, 2, Duration::from_secs(25)).await?;
    foreign_lease.release().await?;
    drop(worker);
    Ok(())
}

/// A heartbeat that stops on its own while the waiter is already asleep — a
/// wind-up lease lost just after the pre-sleep liveness gates passed, staged
/// deterministically by cancelling the heartbeat token at sleep start — must
/// hand the delivery back promptly, not at the next recheck: the recheck can
/// fall past the acknowledgement deadline, letting JetStream redeliver the
/// delivery while the waiter still holds it. The completed heartbeat's
/// handle stays in place, so this also proves the handback's NAK can reap it
/// without re-polling a spent JoinHandle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dead_heartbeat_mid_sleep_hands_the_parked_delivery_back() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "busy-mid-sleep-heartbeat";
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(BusyWaiterProbe::default());
    let faults = Arc::new(ActivationFaults::default());
    // The fault stops the delivery's heartbeat when its waiter starts
    // sleeping, after the pre-sleep liveness gates have passed. Setting it
    // before publishing keeps that deterministic; the lease TTL is the
    // recheck interval (30s), and the bound asserted below is far under one
    // recheck, so the handback can only pass through the sleep observing the
    // heartbeat stopping — not through a recheck whose dead-heartbeat gate
    // would also settle.
    faults.stop_parked_heartbeat.store(true, Ordering::SeqCst);
    let mut daemon = WorkerDaemonConfig::managing("local", "busy-mid-sleep-heartbeat-worker")
        .with_busy_waiter_probe_for_test(Arc::clone(&probe))
        .with_activation_faults_for_test(Arc::clone(&faults));
    daemon.lease = NatsLeaseConfig {
        ttl: Duration::from_secs(30),
        renew_interval: Duration::from_secs(10),
        replicas: 1,
        ..Default::default()
    };
    let readiness = harnx_healthz::Readiness::default();
    let worker = AbortOnDropHandle::new(tokio::spawn(run_worker_daemon(
        local_nats_runtime_config(server.url()),
        daemon,
        Some(counting_stub_call_fn(Arc::clone(&calls))),
        Some(readiness.clone()),
    )));
    wait_until(CI_SAFE_TIMEOUT, move || readiness.is_ready()).await?;

    // The foreign holder keeps renewing, so the waiter parks and sleeps until
    // the fault kills the heartbeat right after the sleep starts. The fault
    // also settles the waiter faster than the `parked` gauge can be polled,
    // so the park is observed through the cumulative counter.
    let foreign_lease =
        publish_foreign_busy_activation(&jetstream, session_id, "foreign-holder").await?;
    let consumer = worker_consumer(&jetstream).await?;
    wait_delivered_sequence(&consumer, 1, CI_SAFE_TIMEOUT).await?;
    let park_probe = Arc::clone(&probe);
    wait_until(CI_SAFE_TIMEOUT, move || {
        park_probe.parked_total.load(Ordering::SeqCst) >= 1
    })
    .await?;

    wait_until(Duration::from_secs(10), move || {
        probe.slots.load(Ordering::SeqCst) == 0 && probe.parked.load(Ordering::SeqCst) == 0
    })
    .await?;
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no turn may launch");
    let stream = jetstream.get_stream("WORK_NOTIFY_local").await?;
    assert_eq!(
        stream.get_info().await?.state.messages,
        1,
        "the parked activation was handed back, not terminated"
    );
    // The handback is a busy NAK: the delivery redelivers within the busy
    // delay instead of staying parked.
    wait_delivered_sequence(&consumer, 2, Duration::from_secs(25)).await?;
    foreign_lease.release().await?;
    drop(worker);
    Ok(())
}
