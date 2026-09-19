use super::*;
use harnx_runtime::nats_lease::NatsLeaseConfig;
use harnx_runtime::nats_worker::{
    publish_session_activate, run_worker_daemon, SessionActivate, WorkerDaemonConfig,
};
use std::future::Future;
use tokio_util::task::AbortOnDropHandle;

type WorkerDaemonHandle = AbortOnDropHandle<Result<()>>;

async fn progress_or_daemon_exit<F, T>(
    label: &str,
    progress: F,
    worker_one: &mut WorkerDaemonHandle,
    worker_two: &mut WorkerDaemonHandle,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    tokio::select! {
        result = progress => result,
        stopped = &mut *worker_one => {
            anyhow::bail!("worker-one daemon stopped during {label}: {stopped:?}")
        }
        stopped = &mut *worker_two => {
            anyhow::bail!("worker-two daemon stopped during {label}: {stopped:?}")
        }
    }
}

fn spawn_test_worker(
    server_url: &str,
    worker_id: &str,
    lease: &NatsLeaseConfig,
    counter: Arc<AtomicUsize>,
) -> (WorkerDaemonHandle, harnx_healthz::Readiness) {
    let mut daemon = WorkerDaemonConfig::managing("local", worker_id);
    daemon.lease = lease.clone();
    let config = local_nats_runtime_config(server_url);
    let readiness = harnx_healthz::Readiness::default();
    let handle = AbortOnDropHandle::new(tokio::spawn({
        let readiness = readiness.clone();
        async move {
            run_worker_daemon(
                config,
                daemon,
                Some(counting_stub_call_fn(counter)),
                Some(readiness),
            )
            .await
        }
    }));
    (handle, readiness)
}

struct WorkerPair {
    worker_one: WorkerDaemonHandle,
    worker_two: WorkerDaemonHandle,
    readiness_one: harnx_healthz::Readiness,
    readiness_two: harnx_healthz::Readiness,
    counter_one: Arc<AtomicUsize>,
    counter_two: Arc<AtomicUsize>,
}

impl WorkerPair {
    fn spawn(server_url: &str, lease: &NatsLeaseConfig) -> Self {
        let counter_one = Arc::new(AtomicUsize::new(0));
        let counter_two = Arc::new(AtomicUsize::new(0));
        let (worker_one, readiness_one) =
            spawn_test_worker(server_url, "worker-one", lease, Arc::clone(&counter_one));
        let (worker_two, readiness_two) =
            spawn_test_worker(server_url, "worker-two", lease, Arc::clone(&counter_two));
        Self {
            worker_one,
            worker_two,
            readiness_one,
            readiness_two,
            counter_one,
            counter_two,
        }
    }

    async fn wait_until_ready(&mut self) -> Result<()> {
        let readiness_one = self.readiness_one.clone();
        let readiness_two = self.readiness_two.clone();
        progress_or_daemon_exit(
            "startup",
            wait_until(CI_SAFE_TIMEOUT, move || {
                readiness_one.is_ready() && readiness_two.is_ready()
            }),
            &mut self.worker_one,
            &mut self.worker_two,
        )
        .await
    }

    async fn wait_for_execution_count(&mut self, label: &str, expected: usize) -> Result<()> {
        let counter_one = Arc::clone(&self.counter_one);
        let counter_two = Arc::clone(&self.counter_two);
        progress_or_daemon_exit(
            label,
            wait_until(CI_SAFE_TIMEOUT, move || {
                counter_one.load(Ordering::SeqCst) + counter_two.load(Ordering::SeqCst) >= expected
            }),
            &mut self.worker_one,
            &mut self.worker_two,
        )
        .await
    }

    async fn wait_for_session_cleanup(
        &mut self,
        label: &str,
        jetstream: &async_nats::jetstream::Context,
        session_id: &str,
    ) -> Result<()> {
        progress_or_daemon_exit(
            label,
            wait_for_worker_session_cleanup(jetstream, session_id),
            &mut self.worker_one,
            &mut self.worker_two,
        )
        .await
    }

    fn execution_count(&self) -> usize {
        self.counter_one.load(Ordering::SeqCst) + self.counter_two.load(Ordering::SeqCst)
    }

    fn assert_running(&self) {
        assert!(
            !self.worker_one.is_finished(),
            "worker-one daemon stopped unexpectedly"
        );
        assert!(
            !self.worker_two.is_finished(),
            "worker-two daemon stopped unexpectedly"
        );
    }

    async fn abort_and_assert_cancelled(self) {
        self.worker_one.abort();
        self.worker_two.abort();
        let stopped_one = self.worker_one.await;
        let stopped_two = self.worker_two.await;
        assert!(
            matches!(&stopped_one, Err(error) if error.is_cancelled()),
            "worker-one daemon did not stop by cancellation: {stopped_one:?}"
        );
        assert!(
            matches!(&stopped_two, Err(error) if error.is_cancelled()),
            "worker-two daemon did not stop by cancellation: {stopped_two:?}"
        );
    }
}

async fn seed_and_publish_activation(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
    user_text: &str,
) -> Result<SessionActivate> {
    NatsSessionLog::new(jetstream.clone(), storage_key(session_id))
        .append_event_async(&SessionLogEntry::Message {
            id: None,
            role: harnx_core::message::MessageRole::User,
            content: harnx_core::message::MessageContent::Text(user_text.to_string()),
            timestamp: None,
            fence_token: None,
        })
        .await?;
    seed_session_metadata(jetstream, session_id).await?;
    let activation = SessionActivate::new(storage_key(session_id));
    publish_session_activate(jetstream, "local", &activation).await?;
    Ok(activation)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workers_share_the_activation_queue_and_dispatch_is_deduplicated() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let fast_lease = NatsLeaseConfig {
        ttl: Duration::from_secs(3),
        renew_interval: Duration::from_millis(500),
        replicas: 1,
        tombstone_ttl: Duration::from_secs(10),
        ..Default::default()
    };
    let mut workers = WorkerPair::spawn(server.url(), &fast_lease);
    workers.wait_until_ready().await?;
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);

    let first_session_id = "dispatch-session-one";
    let first_activation =
        seed_and_publish_activation(&jetstream, first_session_id, "hello worker one").await?;
    publish_session_activate(&jetstream, "local", &first_activation).await?;
    workers
        .wait_for_execution_count("first dispatch", 1)
        .await?;
    workers
        .wait_for_session_cleanup("cleanup after first dispatch", &jetstream, first_session_id)
        .await?;
    let first_total = workers.execution_count();
    assert_eq!(
        first_total, 1,
        "duplicate activation should execute exactly once (got {first_total})"
    );

    let second_session_id = "dispatch-session-two";
    seed_and_publish_activation(&jetstream, second_session_id, "hello worker two").await?;
    workers
        .wait_for_execution_count("second dispatch", 2)
        .await?;
    workers
        .wait_for_session_cleanup(
            "cleanup after second dispatch",
            &jetstream,
            second_session_id,
        )
        .await?;
    let total = workers.execution_count();
    assert_eq!(
        total, 2,
        "each activation should execute once (got {total})"
    );
    workers.assert_running();
    workers.abort_and_assert_cancelled().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fenced_sink_rejects_append_when_lease_lost() -> Result<()> {
    use harnx_runtime::config::session::SessionAppendSink;
    use harnx_runtime::nats_lease::{NatsLeaseConfig, NatsSessionLease};
    use harnx_runtime::nats_worker::FencedSessionLogSink;
    use std::time::Duration;

    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "fenced-session";

    let lease = Arc::new(
        NatsSessionLease::acquire(harnx_runtime::nats_lease::NatsLeaseAcquireParams {
            jetstream: js.clone(),
            session_id: &storage_key(session_id),
            worker_id: "worker-a".to_string(),
            generation: 1,
            config: NatsLeaseConfig {
                ttl: Duration::from_secs(30),
                renew_interval: Duration::from_secs(10),
                ..Default::default()
            },
            session_metadata: None,
        })
        .await?
        .expect("acquire"),
    );

    let backend = generation::fenced_backend(&js, &storage_key(session_id)).await?;
    let sink = FencedSessionLogSink::new(backend, Arc::clone(&lease));

    // Held lease: an assistant append succeeds and is fence-stamped.
    let entry = SessionLogEntry::Message {
        id: None,
        role: harnx_core::message::MessageRole::Assistant,
        content: harnx_core::message::MessageContent::Text("hi".to_string()),
        timestamp: None,
        fence_token: None,
    };
    let fence_at_append = lease.fence_token();
    sink.append(&entry)
        .expect("append while held should succeed");

    // Verify the persisted entry carries the lease revision. A renewal between
    // the read above and the append only advances it, so the stamp is at least
    // what the lease held going in.
    let log = NatsSessionLog::new(js.clone(), storage_key(session_id));
    let loaded = log.load_events_async().await?;
    let stamped = loaded
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::Message { fence_token: Some(f), .. } if *f >= fence_at_append));
    assert!(
        stamped,
        "persisted assistant entry should carry the lease fence (>= {fence_at_append}); loaded={loaded:?}"
    );

    // Lose the lease: subsequent worker append must be rejected (fenced out).
    lease.mark_lost_for_test();
    let rejected = sink.append(&entry);
    assert!(
        rejected.is_err(),
        "append after lease loss must be rejected"
    );
    Ok(())
}

async fn append_resume_fence_seed(log: &NatsSessionLog) -> Result<()> {
    log.append_event_async(&SessionLogEntry::Message {
        id: None,
        role: harnx_core::message::MessageRole::User,
        content: harnx_core::message::MessageContent::Text("go".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    log.append_event_async(&SessionLogEntry::Message {
        id: None,
        role: harnx_core::message::MessageRole::Assistant,
        content: harnx_core::message::MessageContent::Text("from newer worker".to_string()),
        timestamp: None,
        fence_token: Some(u64::MAX),
    })
    .await?;
    Ok(())
}

fn assert_resume_fenced(result: Result<()>) {
    assert!(
        result.is_err(),
        "resume must abort when tail fence exceeds held lease revision"
    );
    let msg = format!("{:#}", result.unwrap_err());
    assert!(
        msg.contains("fenced") || msg.contains("exceeds held lease"),
        "error should indicate fencing, got: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_aborts_when_tail_fence_exceeds_held_revision() -> Result<()> {
    use harnx_runtime::nats_worker::run_agent_loop_with_nats_inner;
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "resume-fence-session";
    let log = NatsSessionLog::new(js.clone(), storage_key(session_id));
    append_resume_fence_seed(&log).await?;
    let lease = acquire_test_lease(js.clone(), session_id, "worker-stale").await?;

    let config = local_nats_runtime_config(server.url());
    let input = harnx_runtime::config::input::from_str(&config, "go", None);
    let result = run_agent_loop_with_nats_inner(
        RunAgentLoopArgs {
            cluster_key: "local",
            manage_servers: false,
            session_id: &storage_key(session_id),
            config,
            instance_id: harnx_core::instance::ServerScope::new(),
            initial_input: input,
            abort_signal: create_abort_signal(),
            token_budget: None,
            call_fn: Some(counting_stub_call_fn(Arc::new(AtomicUsize::new(0)))),
            lease: None,
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
            event_sink: None,
            after_seq_observer: None,
            session_metadata: None,
            on_tool_round: None,
            working_dir: None,
        }
        .with_lease(lease),
    )
    .await;

    assert_resume_fenced(result);
    Ok(())
}
