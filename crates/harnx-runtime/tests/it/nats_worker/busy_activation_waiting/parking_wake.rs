use super::*;

/// While a session runs on this worker, a second activation parks instead of
/// cycling through JetStream, and starts its turn as soon as the running one
/// ends — no redelivery, no busy-NAK delay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_activation_parks_and_runs_when_local_turn_ends() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "busy-local-park";
    let first_prompt = seed_admitted_user(&jetstream, session_id, "run first").await?;
    let (gate, call_fn) = TurnGate::new();
    let worker = spawn_local_worker(
        server.url(),
        "busy-local-park-worker",
        call_fn,
        std::convert::identity,
    )
    .await?;
    publish_session_activate(
        &jetstream,
        "local",
        &SessionActivate::new(storage_key(session_id)).with_requested_seq(first_prompt),
        1,
    )
    .await?;
    gate.wait_until_calls(1).await?;

    // A distinct activation for the busy session arrives and parks.
    publish_activation_for(&jetstream, session_id, "run second").await?;
    let consumer = worker_consumer(&jetstream).await?;
    wait_delivered_sequence(&consumer, 2, CI_SAFE_TIMEOUT).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let info = consumer.get_info().await?;
    assert_eq!(
        info.num_ack_pending, 2,
        "both activations stay held: one running, one parked"
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);

    // Releasing the turn wakes the parked waiter and the second turn starts
    // within a bound shorter than any busy-NAK round trip (10-12s delay plus
    // redelivery), so a NAKed-back delivery cannot pass.
    gate.release.cancel();
    gate.wait_until_calls(2).await?;
    assert_eq!(gate.calls.load(Ordering::SeqCst), 2);

    wait_for_worker_session_cleanup(&jetstream, &storage_key(session_id)).await?;
    let stream = jetstream.get_stream("WORK_NOTIFY_local").await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            if stream.get_info().await?.state.messages == 0 {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    worker.abort();
    let _ = worker.await;
    Ok(())
}

/// An activation for a session whose lease a foreign worker holds parks on
/// this worker, survives multiple recheck cycles without any redelivery, and
/// claims as soon as the holder releases — proving the lease-key watch wake.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_activation_for_foreign_holder_parks_and_claims_on_release() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "busy-foreign-park";
    let prompt = seed_admitted_user(&jetstream, session_id, "wait for holder").await?;
    let foreign_lease =
        acquire_worker_lease(&jetstream, &storage_key(session_id), "foreign-holder").await?;
    let (gate, call_fn) = TurnGate::new();
    let worker = spawn_local_worker(
        server.url(),
        "busy-foreign-park-worker",
        call_fn,
        std::convert::identity,
    )
    .await?;
    publish_session_activate(
        &jetstream,
        "local",
        &SessionActivate::new(storage_key(session_id)).with_requested_seq(prompt),
        1,
    )
    .await?;

    let consumer = worker_consumer(&jetstream).await?;
    wait_delivered_sequence(&consumer, 1, CI_SAFE_TIMEOUT).await?;
    // The recheck timer fires every lease TTL; hold longer than that and the
    // parked waiter must still not disturb the broker.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let info = consumer.get_info().await?;
    assert_eq!(info.num_ack_pending, 1, "the delivery stays held, parked");
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);

    foreign_lease.release().await?;
    // The bound is shorter than any busy-NAK round trip (10-12s delay plus
    // redelivery), so claiming through it proves the parked delivery woke on
    // the lease-key watch rather than a broker redelivery.
    gate.wait_until_calls(1).await?;
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);

    gate.release.cancel();
    wait_for_worker_session_cleanup(&jetstream, &storage_key(session_id)).await?;
    worker.abort();
    let _ = worker.await;
    Ok(())
}

/// A waiter whose lease-key watch is dead must still recover: the recheck
/// timer keeps re-attempting claims while the holder is live (no turn, no
/// broker round trip), and claims within one recheck interval of the
/// release — a bound shorter than any busy-NAK round trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_waiter_survives_a_dropped_watch_on_the_recheck_timer() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(BusyWaiterProbe::default());
    let faults = Arc::new(ActivationFaults::default());
    faults.drop_lease_watch.store(true, Ordering::SeqCst);
    let worker = spawn_fault_probe_worker(
        server.url(),
        FaultWorkerSpec {
            worker_id: "busy-dropped-watch-worker",
            call_fn: counting_stub_call_fn(Arc::clone(&calls)),
            probe: Arc::clone(&probe),
            faults: Arc::clone(&faults),
            per_session_capacity: None,
        },
    )
    .await?;

    let session_id = "busy-dropped-watch";
    let prompt = seed_admitted_user(&jetstream, session_id, "wait without a watch").await?;
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
    // The recheck interval is max(lease ttl, 1s) = 1s here: several full
    // cycles pass with the holder live.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "timer rechecks must not claim from a live holder"
    );
    assert_eq!(
        probe.parked.load(Ordering::SeqCst),
        1,
        "the delivery must stay parked, not handed back"
    );
    assert_eq!(
        consumer.get_info().await?.num_ack_pending,
        1,
        "the parked delivery must stay held by JetStream"
    );

    foreign_lease.release().await?;
    let calls = Arc::clone(&calls);
    wait_until(Duration::from_secs(8), move || {
        calls.load(Ordering::SeqCst) >= 1
    })
    .await?;
    wait_until_registry_empty(&probe).await?;
    wait_until_nothing_pending(&consumer).await?;
    drop(worker);
    Ok(())
}

/// A renewal `Put` on the lease key must not cost the waiter its watch: the
/// watch stays live across renewals, so a foreign release after renewals is
/// picked up through the watch, far inside one recheck interval (the worker's
/// lease TTL), not by the recheck timer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_waiter_keeps_its_lease_watch_across_holder_renewals() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "busy-watch-renewal";
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(BusyWaiterProbe::default());
    let mut daemon = WorkerDaemonConfig::managing("local", "busy-watch-renewal-worker")
        .with_busy_waiter_probe_for_test(Arc::clone(&probe));
    // The recheck interval is the lease TTL: a full recheck costs 30s, so a
    // release-to-claim bound well under it can only pass through the watch.
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

    // The foreign lease renews every 500ms against a 2s TTL, so several
    // renewal Puts land while the delivery is parked.
    let foreign_lease =
        publish_foreign_busy_activation(&jetstream, session_id, "foreign-holder").await?;
    let consumer = worker_consumer(&jetstream).await?;
    wait_delivered_sequence(&consumer, 1, CI_SAFE_TIMEOUT).await?;
    wait_until_parked(&probe, 1).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        probe.parked.load(Ordering::SeqCst),
        1,
        "the renewing holder keeps the delivery parked"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    foreign_lease.release().await?;
    let release_at = std::time::Instant::now();
    wait_until(Duration::from_secs(10), move || {
        calls.load(Ordering::SeqCst) >= 1
    })
    .await?;
    assert!(
        release_at.elapsed() < Duration::from_secs(30),
        "the release was picked up through the retained watch, not a recheck"
    );
    worker.abort();
    let _ = worker.await;
    Ok(())
}
