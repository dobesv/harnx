use super::*;

/// Past the per-session waiter capacity, busy deliveries fall back to the
/// legacy delayed NAK: they keep cycling through the broker, but the parked
/// ones keep their current delivery count while parked, and everything still
/// settles once the running turn ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_waiter_overflow_falls_back_to_delayed_nak() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "busy-park-overflow";
    let first_prompt = seed_admitted_user(&jetstream, session_id, "running turn").await?;
    let (gate, call_fn) = TurnGate::new();
    let worker = spawn_local_worker(
        server.url(),
        "busy-park-overflow-worker",
        call_fn,
        |daemon| daemon.with_busy_wait_limits_for_test(Duration::from_secs(6 * 60 * 60), 1),
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

    publish_activation_for(&jetstream, session_id, "parked turn").await?;
    publish_activation_for(&jetstream, session_id, "overflow turn").await?;
    let consumer = worker_consumer(&jetstream).await?;
    wait_delivered_sequence(&consumer, 3, CI_SAFE_TIMEOUT).await?;
    // The overflowed activation comes back through the broker (a transient
    // claim error can shorten a cycle, so any count proves the fallback).
    tokio::time::sleep(Duration::from_secs(13)).await;
    let info = consumer.get_info().await?;
    assert!(
        info.num_redelivered >= 1,
        "the overflowed activation must fall back to broker redelivery"
    );

    gate.release.cancel();
    // The parked activation must claim through the waiter's release
    // notification. Unlike the dedicated wake tests this test runs alongside
    // nine other broker-backed tests, so the turn only needs to start within
    // the generous bound; the fast-wake property is asserted by the local
    // and foreign-holder tests.
    gate.wait_until_calls_bounded(CI_SAFE_TIMEOUT, 2).await?;
    // The overflowed activation keeps cycling through the broker until the
    // session frees, then comes back on a redelivery and settles. One turn
    // answers both queued prompts, so the overflowed activation needs no
    // turn of its own: it is acknowledged as covered.
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
    let info = consumer.get_info().await?;
    assert_eq!(info.num_ack_pending, 0);
    assert_eq!(
        gate.calls.load(Ordering::SeqCst),
        2,
        "one turn answers both queued prompts"
    );
    worker.abort();
    let _ = worker.await;
    Ok(())
}

/// A busy-wait budget that expires hands the delivery back to JetStream
/// (delayed NAK, deliveries start counting again) instead of parking
/// forever, but never terminates the activation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_wait_budget_expiry_hands_delivery_back() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "busy-park-budget";
    let prompt = seed_admitted_user(&jetstream, session_id, "stuck session").await?;
    let foreign_lease =
        acquire_worker_lease(&jetstream, &storage_key(session_id), "foreign-holder").await?;
    let (gate, call_fn) = TurnGate::new();
    let worker = spawn_local_worker(server.url(), "busy-park-budget-worker", call_fn, |daemon| {
        daemon.with_busy_wait_limits_for_test(Duration::from_millis(300), 4)
    })
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
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        gate.calls.load(Ordering::SeqCst),
        0,
        "parks before the budget"
    );

    // After the budget expires the activation cycles through the broker
    // again — without ever being terminated while the session stays busy.
    wait_delivered_sequence(&consumer, 2, Duration::from_secs(30)).await?;
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
    let stream = jetstream.get_stream("WORK_NOTIFY_local").await?;
    assert_eq!(stream.get_info().await?.state.messages, 1);

    foreign_lease.release().await?;
    worker.abort();
    let _ = worker.await;
    Ok(())
}

/// A finished waiter's session slot must leave the registry: parking and
/// settling deliveries for one session after another cannot accumulate
/// slots, watch channels or parked counts behind it. The registry probe is
/// the assertion — a slot that lingers past its last parked waiter fails
/// this test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_waiter_slots_do_not_leak_across_finished_sessions() -> Result<()> {
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
            worker_id: "busy-slot-leak-worker",
            call_fn: counting_stub_call_fn(Arc::clone(&calls)),
            probe: Arc::clone(&probe),
            faults: Arc::clone(&faults),
            per_session_capacity: None,
        },
    )
    .await?;

    for index in 0..3 {
        let session_id = format!("busy-slot-leak-{index}");
        let prompt = seed_admitted_user(&jetstream, &session_id, "park then run").await?;
        let foreign_lease = acquire_worker_lease(
            &jetstream,
            &storage_key(&session_id),
            &format!("foreign-holder-{index}"),
        )
        .await?;
        publish_session_activate(
            &jetstream,
            "local",
            &SessionActivate::new(storage_key(&session_id)).with_requested_seq(prompt),
            1,
        )
        .await?;
        wait_until_parked(&probe, 1).await?;
        foreign_lease.release().await?;
        // The waiter claims, runs its turn, and deregisters: both the parked
        // count and the slot must be gone before the next session parks.
        let expected_calls = index + 1;
        let calls = Arc::clone(&calls);
        wait_until(CI_SAFE_TIMEOUT, move || {
            calls.load(Ordering::SeqCst) >= expected_calls
        })
        .await?;
        wait_until_registry_empty(&probe).await?;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    drop(worker);
    Ok(())
}

/// The worker-wide waiter bound is a distinct resource from the per-session
/// cap: with one slot per session, eight distinct sessions fill the worker
/// total (per-session capacity × admission concurrency), and a ninth busy
/// session must fall back to a broker redelivery instead of parking.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_waiter_total_capacity_overflows_across_distinct_sessions() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(BusyWaiterProbe::default());
    let faults = Arc::new(ActivationFaults::default());
    // Per-session capacity one: the worker total is 1 × MAX_CONCURRENT_ADMISSIONS (8).
    let worker = spawn_fault_probe_worker(
        server.url(),
        FaultWorkerSpec {
            worker_id: "busy-total-overflow-worker",
            call_fn: counting_stub_call_fn(Arc::clone(&calls)),
            probe: Arc::clone(&probe),
            faults: Arc::clone(&faults),
            per_session_capacity: Some(1),
        },
    )
    .await?;

    let mut foreign_leases = Vec::new();
    for index in 0..9 {
        let session_id = format!("busy-total-overflow-{index}");
        foreign_leases.push(
            publish_foreign_busy_activation(
                &jetstream,
                &session_id,
                &format!("foreign-holder-{index}"),
            )
            .await?,
        );
    }
    let consumer = worker_consumer(&jetstream).await?;
    wait_delivered_sequence(&consumer, 9, CI_SAFE_TIMEOUT).await?;
    // Eight distinct sessions park; the ninth overflows the worker total
    // and falls back to the broker (10-12s delay) even though no single
    // session is at its own cap.
    wait_until_parked(&probe, 8).await?;
    assert_eq!(probe.slots.load(Ordering::SeqCst), 8);
    tokio::time::sleep(Duration::from_secs(13)).await;
    assert!(
        consumer.get_info().await?.num_redelivered >= 1,
        "the ninth busy session must fall back to broker redelivery"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "nothing may claim while every holder is foreign"
    );
    assert_eq!(
        probe.parked.load(Ordering::SeqCst),
        8,
        "the overflowed delivery must not have parked"
    );

    for lease in foreign_leases {
        lease.release().await?;
    }
    // Eight waiters wake on their lease watches; the ninth comes back on its
    // redelivery. Every session eventually settles.
    let calls = Arc::clone(&calls);
    wait_until(CI_SAFE_TIMEOUT, move || calls.load(Ordering::SeqCst) >= 9).await?;
    wait_until_registry_empty(&probe).await?;
    wait_until_nothing_pending(&consumer).await?;
    drop(worker);
    Ok(())
}

/// The busy-wait budget caps the waiter's sleep, not just its top-of-loop
/// check: with a recheck interval far past the budget, the waiter must wake
/// at budget expiry and hand its delivery back instead of sleeping a whole
/// lease TTL past it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_wait_budget_expiry_wakes_the_waiter_before_a_recheck_interval() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "busy-budget-wake";
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(BusyWaiterProbe::default());
    let mut daemon = WorkerDaemonConfig::managing("local", "busy-budget-wake-worker")
        .with_busy_wait_limits_for_test(Duration::from_secs(2), 4)
        .with_busy_waiter_probe_for_test(Arc::clone(&probe));
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

    let foreign_lease =
        publish_foreign_busy_activation(&jetstream, session_id, "foreign-holder").await?;
    let consumer = worker_consumer(&jetstream).await?;
    wait_delivered_sequence(&consumer, 1, CI_SAFE_TIMEOUT).await?;
    wait_until_parked(&probe, 1).await?;

    // The budget (2s) is far inside one recheck interval (the 30s lease
    // TTL), and the foreign lease stays held, so nothing but the budget can
    // wake the waiter. A waiter that slept a full recheck would still be
    // parked here; handing the delivery back deregisters right away.
    wait_until(Duration::from_secs(10), move || {
        probe.slots.load(Ordering::SeqCst) == 0
    })
    .await?;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the session never freed, so no turn ran"
    );
    foreign_lease.release().await?;
    worker.abort();
    let _ = worker.await;
    Ok(())
}
