use super::support::{spawn_test_worker, WorkerPair};
use super::*;
use harnx_runtime::nats_lease::NatsLeaseConfig;
use harnx_runtime::nats_worker::{
    publish_session_activate, run_worker_daemon, SessionActivate, WorkerDaemonConfig,
};
use tokio_util::task::AbortOnDropHandle;

async fn seed_and_publish_activation(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
    user_text: &str,
) -> Result<SessionActivate> {
    NatsSessionLog::new_with_replicas(jetstream.clone(), storage_key(session_id), 1)
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
    publish_session_activate(jetstream, "local", &activation, 1).await?;
    Ok(activation)
}
fn fast_lease_config() -> NatsLeaseConfig {
    NatsLeaseConfig {
        ttl: Duration::from_secs(3),
        renew_interval: Duration::from_millis(500),
        replicas: 1,
        tombstone_ttl: Duration::from_secs(10),
        ..Default::default()
    }
}

struct GatedCallFn {
    entered: Arc<tokio::sync::Notify>,
    release: tokio_util::sync::CancellationToken,
    calls: Arc<AtomicUsize>,
}

impl GatedCallFn {
    fn new(reply_text: &'static str) -> (Self, harnx_runtime::agent_loop::AgentCallFn) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = tokio_util::sync::CancellationToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let call_fn: harnx_runtime::agent_loop::AgentCallFn = Arc::new({
            let entered = Arc::clone(&entered);
            let release = release.clone();
            let calls = Arc::clone(&calls);
            move |_input, _config, _abort| {
                let entered = Arc::clone(&entered);
                let release = release.clone();
                let calls = Arc::clone(&calls);
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    entered.notify_one();
                    release.cancelled().await;
                    Ok((
                        reply_text.to_string(),
                        None,
                        vec![],
                        harnx_runtime::client::CompletionTokenUsage::default(),
                    ))
                })
            }
        });
        (
            Self {
                entered,
                release,
                calls,
            },
            call_fn,
        )
    }

    async fn wait_entered(&self, worker: &mut AbortOnDropHandle<Result<()>>) -> Result<()> {
        tokio::time::timeout(CI_SAFE_TIMEOUT, async {
            tokio::select! {
                _ = self.entered.notified() => Ok::<_, anyhow::Error>(()),
                stopped = worker => anyhow::bail!("worker stopped during admission: {stopped:?}"),
            }
        })
        .await??;
        Ok(())
    }
}

/// Broker routing for observing the worker's activation delivery queue.
#[derive(Clone, Copy)]
struct ConsumerTarget {
    cluster: &'static str,
    consumer: &'static str,
}

const LOCAL_WORKER_CONSUMER: ConsumerTarget = ConsumerTarget {
    cluster: "local",
    consumer: "workers",
};

async fn get_worker_consumer(
    jetstream: &async_nats::jetstream::Context,
    target: ConsumerTarget,
) -> Result<async_nats::jetstream::consumer::PullConsumer> {
    let stream = jetstream
        .get_stream(format!("WORK_NOTIFY_{}", target.cluster))
        .await?;
    stream
        .get_consumer(target.consumer)
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

async fn wait_for_consumer_delivered_sequence(
    jetstream: &async_nats::jetstream::Context,
    target: ConsumerTarget,
    min_sequence: u64,
) -> Result<async_nats::jetstream::consumer::PullConsumer> {
    let consumer = get_worker_consumer(jetstream, target).await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            if consumer.get_info().await?.delivered.consumer_sequence >= min_sequence {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    Ok(consumer)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workers_share_the_activation_queue_and_dispatch_is_deduplicated() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let fast_lease = fast_lease_config();
    let mut workers = WorkerPair::spawn(server.url(), &fast_lease);
    workers.wait_until_ready().await?;
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);

    let first_session_id = "dispatch-session-one";
    let first_activation =
        seed_and_publish_activation(&jetstream, first_session_id, "hello worker one").await?;
    publish_session_activate(&jetstream, "local", &first_activation, 1).await?;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn long_running_turn_keeps_one_activation_delivery_past_ack_wait() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let ack_wait = Duration::from_secs(1);
    let (gate, call_fn) = GatedCallFn::new("heartbeat complete");
    let mut daemon = WorkerDaemonConfig::managing("local", "heartbeat-worker")
        .with_activation_ack_wait_for_test(ack_wait);
    daemon.lease.replicas = 1;
    let readiness = harnx_healthz::Readiness::default();
    let mut worker = AbortOnDropHandle::new(tokio::spawn(run_worker_daemon(
        local_nats_runtime_config(server.url()),
        daemon,
        Some(call_fn),
        Some(readiness.clone()),
    )));
    wait_until(CI_SAFE_TIMEOUT, move || readiness.is_ready()).await?;

    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    seed_and_publish_activation(&jetstream, "heartbeat-session", "stay running").await?;
    gate.wait_entered(&mut worker).await?;

    tokio::time::sleep(ack_wait * 4).await;
    let consumer = get_worker_consumer(&jetstream, LOCAL_WORKER_CONSUMER).await?;
    let info = consumer.get_info().await?;
    assert_eq!(
        info.delivered.consumer_sequence, 1,
        "heartbeat must keep activation on its first delivery"
    );
    assert_eq!(info.num_redelivered, 0);
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);

    gate.release.cancel();
    wait_for_worker_session_cleanup(&jetstream, "heartbeat-session").await?;
    worker.abort();
    let stopped = worker.await;
    assert!(matches!(stopped, Err(error) if error.is_cancelled()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_same_session_activations_start_one_claim() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "concurrent-same-session";
    let first = seed_and_publish_activation(&jetstream, session_id, "run once").await?;
    let mut second = first.clone();
    second.epoch = "concurrent-second-activation".to_string();
    publish_session_activate(&jetstream, "local", &second, 1).await?;

    let (gate, call_fn) = GatedCallFn::new("one claim");
    let mut daemon = WorkerDaemonConfig::managing("local", "reservation-worker");
    daemon.lease.replicas = 1;
    let readiness = harnx_healthz::Readiness::default();
    let worker = AbortOnDropHandle::new(tokio::spawn(run_worker_daemon(
        local_nats_runtime_config(server.url()),
        daemon,
        Some(call_fn),
        Some(readiness.clone()),
    )));
    wait_until(CI_SAFE_TIMEOUT, move || readiness.is_ready()).await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, gate.entered.notified()).await?;

    wait_for_consumer_delivered_sequence(&jetstream, LOCAL_WORKER_CONSUMER, 2).await?;
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);

    gate.release.cancel();
    wait_for_worker_session_cleanup(&jetstream, session_id).await?;
    worker.abort();
    let stopped = worker.await;
    assert!(matches!(stopped, Err(error) if error.is_cancelled()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreign_lease_defers_before_metadata_or_full_log_preflight() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = storage_key("foreign-lease-cheap-preflight");
    let foreign_lease = acquire_worker_lease(&jetstream, &session_id, "foreign-worker").await?;
    seed_session_metadata(&jetstream, &session_id).await?;
    let activation = SessionActivate::new(&session_id);
    publish_session_activate(&jetstream, "local", &activation, 1).await?;

    // Deliberately don't create a session-log stream. Reaching full-log
    // preflight would create/read that stream before the authoritative CAS.
    let calls = Arc::new(AtomicUsize::new(0));
    let (worker, readiness) = spawn_test_worker(
        server.url(),
        "cheap-preflight-worker",
        &NatsLeaseConfig::default(),
        Arc::clone(&calls),
    );
    wait_until(CI_SAFE_TIMEOUT, move || readiness.is_ready()).await?;
    wait_for_consumer_delivered_sequence(&jetstream, LOCAL_WORKER_CONSUMER, 1).await?;
    let stream = jetstream.get_stream("WORK_NOTIFY_local").await?;

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        stream.get_info().await?.state.messages,
        1,
        "foreign lease must busy-NAK before metadata or full-log preflight"
    );
    assert!(
        jetstream
            .get_stream(&harnx_runtime::nats_session_log::stream_name_for_session(
                &session_id,
            ))
            .await
            .is_err(),
        "cheap busy path must not create a session-log stream"
    );

    foreign_lease.release().await?;
    worker.abort();
    let stopped = worker.await;
    assert!(matches!(stopped, Err(error) if error.is_cancelled()));
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
    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key(session_id), 1);
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
    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key(session_id), 1);
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
