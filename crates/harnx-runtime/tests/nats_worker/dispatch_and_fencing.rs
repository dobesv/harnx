use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_runs_exactly_one_worker_per_activation_and_reactivation_is_noop() -> Result<()> {
    use harnx_runtime::nats_lease::NatsLeaseConfig;
    use harnx_runtime::nats_worker::{
        publish_session_activate, run_worker_daemon, SessionActivate, WorkerDaemonConfig,
    };
    use std::time::Duration;

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

    let counter_one = Arc::new(AtomicUsize::new(0));
    let counter_two = Arc::new(AtomicUsize::new(0));

    let mut cfg_one = WorkerDaemonConfig::managing("local", "worker-one");
    cfg_one.lease = fast_lease.clone();
    let mut cfg_two = WorkerDaemonConfig::managing("local", "worker-two");
    cfg_two.lease = fast_lease.clone();

    let config_one = local_nats_runtime_config(server.url());
    let config_two = local_nats_runtime_config(server.url());

    let h1 = tokio::spawn({
        let c = config_one.clone();
        let call = counting_stub_call_fn(counter_one.clone());
        async move { run_worker_daemon(c, cfg_one, Some(call), None).await }
    });
    let h2 = tokio::spawn({
        let c = config_two.clone();
        let call = counting_stub_call_fn(counter_two.clone());
        async move { run_worker_daemon(c, cfg_two, Some(call), None).await }
    });

    // Give the daemons a moment to subscribe.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);

    // Client appends a user message to the session log, then activates it.
    let session_id = "dispatch-session";
    let client_log = NatsSessionLog::new(js.clone(), storage_key(session_id));
    client_log
        .append_event_async(&SessionLogEntry::Message {
            id: None,
            role: harnx_core::message::MessageRole::User,
            content: harnx_core::message::MessageContent::Text("hello worker".to_string()),
            timestamp: None,
            fence_token: None,
        })
        .await?;

    seed_session_metadata(&js, session_id).await?;
    let activation = SessionActivate::new(storage_key(session_id));
    publish_session_activate(&js, "local", &activation).await?;
    // Duplicate publish is deduped by Nats-Msg-Id; still only one execution.
    publish_session_activate(&js, "local", &activation).await?;

    // Exactly one worker executes one turn.
    wait_until(CI_SAFE_TIMEOUT, || {
        counter_one.load(Ordering::SeqCst) + counter_two.load(Ordering::SeqCst) >= 1
    })
    .await?;
    // Let any erroneous second executor surface.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let total = counter_one.load(Ordering::SeqCst) + counter_two.load(Ordering::SeqCst);
    assert_eq!(
        total, 1,
        "exactly one worker should execute the activation (got {total})"
    );

    h1.abort();
    h2.abort();
    let _ = h1.await;
    let _ = h2.await;
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
