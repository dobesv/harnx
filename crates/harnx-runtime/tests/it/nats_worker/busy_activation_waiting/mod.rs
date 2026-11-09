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

mod capacity;
mod faults_settlement;
mod parking_wake;
mod shutdown;
