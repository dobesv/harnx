//! Busy-activation parking (`daemon_runtime/busy_wait.rs`): a busy session
//! must not spend its queued activations' delivery budget on broker round
//! trips, must wake as soon as the session frees up (same worker or foreign
//! holder), must fall back to a delayed NAK once its waiter capacity or
//! busy-wait budget is spent, and must hand parked deliveries back on
//! shutdown.

use super::*;
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

/// Lease timings whose recheck cadence (the TTL) fits inside a test.
fn fast_daemon_lease() -> NatsLeaseConfig {
    NatsLeaseConfig {
        ttl: Duration::from_secs(1),
        renew_interval: Duration::from_millis(250),
        replicas: 1,
        ..Default::default()
    }
}

/// A call_fn that blocks every turn until `release` is cancelled, counting
/// turns as it goes.
struct TurnGate {
    release: CancellationToken,
    calls: Arc<AtomicUsize>,
}

impl TurnGate {
    fn new() -> (Self, harnx_runtime::agent_loop::AgentCallFn) {
        let entered = Arc::new(Notify::new());
        let release = CancellationToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let call_fn: harnx_runtime::agent_loop::AgentCallFn = {
            let entered = Arc::clone(&entered);
            let release = release.clone();
            let calls = Arc::clone(&calls);
            Arc::new(move |_input, _config, _abort| {
                let entered = Arc::clone(&entered);
                let release = release.clone();
                let calls = Arc::clone(&calls);
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    entered.notify_one();
                    release.cancelled().await;
                    Ok((
                        "done".to_string(),
                        None,
                        vec![],
                        CompletionTokenUsage::default(),
                    ))
                })
            })
        };
        (Self { release, calls }, call_fn)
    }

    /// Wait for the Nth turn to start. The bound is deliberately below the
    /// old busy-NAK round trip (10-12s delay plus redelivery): a parked
    /// waiter that wakes through this timeout cannot be relying on the
    /// broker to bring the activation back.
    async fn wait_until_calls(&self, expected: usize) -> Result<()> {
        self.wait_until_calls_bounded(Duration::from_secs(8), expected)
            .await
    }

    async fn wait_until_calls_bounded(&self, bound: Duration, expected: usize) -> Result<()> {
        tokio::time::timeout(bound, async {
            loop {
                if self.calls.load(Ordering::SeqCst) >= expected {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        Ok(())
    }
}

async fn worker_consumer(
    jetstream: &async_nats::jetstream::Context,
) -> Result<async_nats::jetstream::consumer::PullConsumer> {
    let stream = jetstream.get_stream("WORK_NOTIFY_local").await?;
    stream
        .get_consumer("workers")
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

async fn wait_delivered_sequence(
    consumer: &async_nats::jetstream::consumer::PullConsumer,
    minimum: u64,
    timeout: Duration,
) -> Result<()> {
    tokio::time::timeout(timeout, async {
        loop {
            if consumer.get_info().await?.delivered.consumer_sequence >= minimum {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    Ok(())
}

/// Seed metadata plus one admitted user message, returning the message's
/// sequence so tests can publish activations targeting distinct prompts.
async fn seed_admitted_user(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
    text: &str,
) -> Result<u64> {
    seed_session_metadata(jetstream, session_id).await?;
    let log = NatsSessionLog::new_with_replicas(jetstream.clone(), storage_key(session_id), 1);
    crate::worker::append_admitted_fixture_user(&log, &uuid::Uuid::new_v4().to_string(), text).await
}

/// Queue one prompt and publish an activation for it: the one-line form of
/// `seed_admitted_user` + `publish_session_activate` every busy-waiter test
/// needs.
async fn publish_activation_for(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
    text: &str,
) -> Result<u64> {
    let prompt = seed_admitted_user(jetstream, session_id, text).await?;
    publish_session_activate(
        jetstream,
        "local",
        &SessionActivate::new(storage_key(session_id)).with_requested_seq(prompt),
        1,
    )
    .await
}

/// Start a local worker daemon against the test server and wait for readiness.
/// `tune` customizes the daemon config (busy-wait limits and the like) before
/// the worker spawns.
async fn spawn_local_worker(
    server_url: &str,
    worker_id: &str,
    call_fn: harnx_runtime::agent_loop::AgentCallFn,
    tune: impl FnOnce(WorkerDaemonConfig) -> WorkerDaemonConfig,
) -> Result<AbortOnDropHandle<Result<()>>> {
    let mut daemon = tune(WorkerDaemonConfig::managing("local", worker_id));
    daemon.lease = fast_daemon_lease();
    let readiness = harnx_healthz::Readiness::default();
    let worker = AbortOnDropHandle::new(tokio::spawn(run_worker_daemon(
        local_nats_runtime_config(server_url),
        daemon,
        Some(call_fn),
        Some(readiness.clone()),
    )));
    wait_until(CI_SAFE_TIMEOUT, move || readiness.is_ready()).await?;
    Ok(worker)
}

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

/// Helper for the fault- and probe-driven busy-waiter tests: a worker whose
/// busy-waiter registry is observable (`BusyWaiterProbe`) and whose claim
/// chain carries injectable faults.
struct FaultWorkerSpec {
    worker_id: &'static str,
    call_fn: harnx_runtime::agent_loop::AgentCallFn,
    probe: Arc<BusyWaiterProbe>,
    faults: Arc<ActivationFaults>,
    per_session_capacity: Option<usize>,
}

async fn spawn_fault_probe_worker(
    server_url: &str,
    spec: FaultWorkerSpec,
) -> Result<AbortOnDropHandle<Result<()>>> {
    let FaultWorkerSpec {
        worker_id,
        call_fn,
        probe,
        faults,
        per_session_capacity,
    } = spec;
    let mut daemon = WorkerDaemonConfig::managing("local", worker_id);
    daemon.lease = fast_daemon_lease();
    let daemon = daemon
        .with_busy_waiter_probe_for_test(Arc::clone(&probe))
        .with_activation_faults_for_test(Arc::clone(&faults));
    let daemon = if let Some(capacity) = per_session_capacity {
        daemon.with_busy_wait_limits_for_test(Duration::from_secs(6 * 60 * 60), capacity)
    } else {
        daemon
    };
    let readiness = harnx_healthz::Readiness::default();
    let worker = AbortOnDropHandle::new(tokio::spawn(run_worker_daemon(
        local_nats_runtime_config(server_url),
        daemon,
        Some(call_fn),
        Some(readiness.clone()),
    )));
    wait_until(CI_SAFE_TIMEOUT, move || readiness.is_ready()).await?;
    Ok(worker)
}

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

/// Queue a prompt behind a foreign-held lease and publish its activation, so
/// the delivery parks on a busy waiter instead of claiming.
async fn publish_foreign_busy_activation(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
    holder_id: &str,
) -> Result<Arc<harnx_runtime::nats_lease::NatsSessionLease>> {
    let foreign_lease =
        acquire_worker_lease(jetstream, &storage_key(session_id), holder_id).await?;
    publish_activation_for(jetstream, session_id, "park behind a foreign lease").await?;
    Ok(foreign_lease)
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

/// Wait until the probe reports exactly this many parked waiters.
async fn wait_until_parked(probe: &Arc<BusyWaiterProbe>, expected: usize) -> Result<()> {
    let probe = Arc::clone(probe);
    wait_until(CI_SAFE_TIMEOUT, move || {
        probe.parked.load(Ordering::SeqCst) == expected
    })
    .await
}

/// Wait until the probe reports the registry completely empty.
async fn wait_until_registry_empty(probe: &Arc<BusyWaiterProbe>) -> Result<()> {
    let probe = Arc::clone(probe);
    wait_until(CI_SAFE_TIMEOUT, move || {
        probe.slots.load(Ordering::SeqCst) == 0 && probe.parked.load(Ordering::SeqCst) == 0
    })
    .await
}

/// Wait until the worker's consumer has no delivery pending: every
/// activation the test published has been settled.
async fn wait_until_nothing_pending(
    consumer: &async_nats::jetstream::consumer::PullConsumer,
) -> Result<()> {
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            if consumer.get_info().await?.num_ack_pending == 0 {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
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
