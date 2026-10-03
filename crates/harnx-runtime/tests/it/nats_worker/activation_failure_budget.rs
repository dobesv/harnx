use super::*;
use anyhow::Context;
use futures_util::StreamExt;

async fn seed_and_publish_activation(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
    user_text: &str,
) -> Result<SessionActivate> {
    NatsSessionLog::new_with_replicas(jetstream.clone(), storage_key(session_id), 1)
        .append_event_async(&SessionLogEntry::Message {
            id: None,
            role: MessageRole::User,
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
async fn wait_for_terminal_activation(
    consumer: &async_nats::jetstream::consumer::PullConsumer,
    stream: &async_nats::jetstream::stream::Stream,
    minimum_consumer_sequence: u64,
) -> Result<()> {
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            let info = consumer.get_info().await?;
            if info.delivered.consumer_sequence < minimum_consumer_sequence {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            if info.num_ack_pending != 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            if stream.get_info().await?.state.messages != 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            return Ok::<_, anyhow::Error>(());
        }
    })
    .await??;
    Ok(())
}

async fn wait_for_delivery_count(
    consumer: &async_nats::jetstream::consumer::PullConsumer,
    minimum_consumer_sequence: u64,
) -> Result<()> {
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            if consumer.get_info().await?.delivered.consumer_sequence >= minimum_consumer_sequence {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pre_turn_infrastructure_failure_terminates_after_ten_failures() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "worker-retry-limit",
        counting_stub_call_fn(Arc::clone(&calls)),
    )
    .await?;
    let jetstream = local_test_nats(server.url()).await?;
    let session_id = "retryable-delivery-limit";
    seed_session_metadata(&jetstream, session_id).await?;
    let storage_key = storage_key(session_id);
    let log = NatsSessionLog::new_with_replicas(jetstream.clone(), &storage_key, 1);
    log.append_event_async(&SessionLogEntry::Message {
        id: Some("infrastructure-failure-user".to_string()),
        role: harnx_core::message::MessageRole::User,
        content: harnx_core::message::MessageContent::Text("fail before turn".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    // Worker retains its opened metadata store, but every preflight read now
    // fails. No turn or model call can start.
    jetstream
        .delete_key_value(harnx_runtime::nats_session_metadata::SESSION_METADATA_BUCKET)
        .await?;
    publish_session_activate(&jetstream, "local", &SessionActivate::new(&storage_key), 1).await?;

    let stream = jetstream.get_stream("WORK_NOTIFY_local").await?;
    let consumer: async_nats::jetstream::consumer::PullConsumer = stream
        .get_consumer("workers")
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    wait_for_terminal_activation(&consumer, &stream, 10).await?;

    assert_eq!(calls.load(Ordering::SeqCst), 0, "turn must never start");
    let errors = log
        .load_events_async()
        .await?
        .into_iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Error { message, .. } => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        errors.len(),
        1,
        "only terminal retry writes a durable error"
    );
    assert!(
        errors[0].contains("metadata"),
        "terminal infrastructure error should explain metadata read failure: {}",
        errors[0]
    );

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

async fn redeliver_activation(
    consumer: &async_nats::jetstream::consumer::PullConsumer,
) -> Result<i64> {
    let mut batch = consumer
        .fetch()
        .max_messages(1)
        .expires(Duration::from_secs(5))
        .messages()
        .await?;
    let message = batch
        .next()
        .await
        .context("activation fetch returned no message")?
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let delivered = message
        .info()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?
        .delivered;
    message
        .ack_with(async_nats::jetstream::AckKind::Nak(None))
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(delivered)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_activation_at_high_delivery_count_is_not_terminated() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "busy-high-delivery-count";
    seed_and_publish_activation(&jetstream, session_id, "stay pending").await?;
    let stream = jetstream.get_stream("WORK_NOTIFY_local").await?;
    let consumer = legacy_activation_consumer(&stream).await?;
    for expected in 1..10 {
        assert_eq!(redeliver_activation(&consumer).await?, expected);
    }

    let foreign_lease =
        acquire_worker_lease(&jetstream, &storage_key(session_id), "foreign-worker").await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let worker = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "busy-worker",
        counting_stub_call_fn(Arc::clone(&calls)),
    )
    .await?;

    wait_for_delivery_count(&consumer, 10).await?;
    assert_eq!(
        stream.get_info().await?.state.messages,
        1,
        "busy activation must remain queued even at the failure delivery limit"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    foreign_lease.release().await?;
    jetstream
        .delete_key_value(harnx_runtime::nats_session_metadata::SESSION_METADATA_BUCKET)
        .await?;
    wait_for_delivery_count(&consumer, 11).await?;
    let log = NatsSessionLog::new_with_replicas(jetstream.clone(), storage_key(session_id), 1);
    assert_eq!(
        stream.get_info().await?.state.messages,
        1,
        "first genuine failure after a high-count busy NAK must stay queued"
    );
    assert!(!log
        .load_events_async()
        .await?
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::Error { .. })));

    worker.abort();
    let stopped = worker.await;
    assert!(matches!(stopped, Err(error) if error.is_cancelled()));
    Ok(())
}

/// Freezing the run's limits fails on every attempt here, as it does when the
/// limits record is unreadable. That failure used to skip the failure budget,
/// so the activation was redelivered for as long as the session existed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_limit_failure_terminates_after_ten_failures() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "worker-run-limit-failure",
        counting_stub_call_fn(Arc::clone(&calls)),
    )
    .await?;
    let jetstream = local_test_nats(server.url()).await?;
    let session_id = "unreadable-run-limits";
    let (store, _) = seed_session_metadata(&jetstream, session_id).await?;
    let storage_key = storage_key(session_id);
    let log = NatsSessionLog::new_with_replicas(jetstream.clone(), &storage_key, 1);
    let prompt_seq =
        crate::worker::append_admitted_fixture_user(&log, "limits-user", "fail before turn")
            .await?;
    let admission = store
        .prompt_admission(&storage_key, "limits-user")
        .await?
        .context("fixture prompt admission")?;
    store
        .kv_store()
        .put(
            format!(
                "sessions/{storage_key}/invocations/{}",
                admission.invocation_id.as_str()
            ),
            "not a limits record".into(),
        )
        .await?;
    publish_session_activate(
        &jetstream,
        "local",
        &SessionActivate::new(&storage_key).with_requested_seq(prompt_seq),
        1,
    )
    .await?;

    let stream = jetstream.get_stream("WORK_NOTIFY_local").await?;
    let consumer: async_nats::jetstream::consumer::PullConsumer = stream
        .get_consumer("workers")
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    wait_for_terminal_activation(&consumer, &stream, 10).await?;

    assert_eq!(calls.load(Ordering::SeqCst), 0, "turn must never start");
    let errors = log
        .load_events_async()
        .await?
        .into_iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Error { message, .. } => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1, "only the last failure writes an error");
    assert!(
        errors[0].contains("Invalid limits record"),
        "the error should say what failed: {}",
        errors[0]
    );
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

async fn legacy_activation_consumer(
    stream: &async_nats::jetstream::stream::Stream,
) -> Result<async_nats::jetstream::consumer::PullConsumer> {
    stream
        .get_or_create_consumer(
            "workers",
            async_nats::jetstream::consumer::pull::Config {
                durable_name: Some("workers".to_string()),
                filter_subject: harnx_runtime::nats_worker::notify_subject("local"),
                ack_wait: Duration::from_secs(30),
                max_deliver: -1,
                inactive_threshold: Duration::from_secs(60 * 60),
                ..Default::default()
            },
        )
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

/// Deferring an activation while its session is busy used to have no limit,
/// and activations for a session that stayed busy were redelivered for days.
/// The deferral that would take an activation past its last allowed delivery
/// terminates it instead. Whoever holds the session is left alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_activation_at_the_delivery_limit_is_terminated() -> Result<()> {
    use harnx_runtime::nats_worker::MAX_ACTIVATION_DELIVERIES;
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "busy-past-delivery-limit";
    seed_and_publish_activation(&jetstream, session_id, "never claimed").await?;
    let stream = jetstream.get_stream("WORK_NOTIFY_local").await?;
    let consumer = legacy_activation_consumer(&stream).await?;
    for expected in 1..MAX_ACTIVATION_DELIVERIES {
        assert_eq!(redeliver_activation(&consumer).await?, expected);
    }

    let foreign_lease =
        acquire_worker_lease(&jetstream, &storage_key(session_id), "foreign-worker").await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let worker = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "delivery-limit-worker",
        counting_stub_call_fn(Arc::clone(&calls)),
    )
    .await?;

    wait_for_terminal_activation(&consumer, &stream, MAX_ACTIVATION_DELIVERIES as u64).await?;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        foreign_lease.is_held(),
        "the worker holding the session keeps it"
    );
    let log = NatsSessionLog::new_with_replicas(jetstream.clone(), storage_key(session_id), 1);
    assert!(
        !log.load_events_async()
            .await?
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::Error { .. })),
        "the session belongs to the lease holder, which hasn't failed"
    );

    foreign_lease.release().await?;
    worker.abort();
    let stopped = worker.await;
    assert!(matches!(stopped, Err(error) if error.is_cancelled()));
    Ok(())
}

/// A consumer created before workers limited redelivery has no limit, and
/// opening it doesn't change that. A worker starting on such a cluster gives
/// it the delivery limit and backoff.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_limits_redelivery_on_an_existing_consumer() -> Result<()> {
    use harnx_runtime::nats_worker::MAX_ACTIVATION_DELIVERIES;
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let stream = jetstream
        .create_stream(async_nats::jetstream::stream::Config {
            name: "WORK_NOTIFY_local".to_string(),
            subjects: vec![harnx_runtime::nats_worker::notify_subject("local")],
            retention: async_nats::jetstream::stream::RetentionPolicy::WorkQueue,
            storage: async_nats::jetstream::stream::StorageType::File,
            ..Default::default()
        })
        .await?;
    let mut consumer = legacy_activation_consumer(&stream).await?;
    assert_eq!(consumer.cached_info().config.max_deliver, -1);

    let worker = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "consumer-limit-worker",
        counting_stub_call_fn(Arc::new(AtomicUsize::new(0))),
    )
    .await?;

    let configured = consumer.info().await?.config.clone();
    assert!(
        configured.max_deliver > MAX_ACTIVATION_DELIVERIES,
        "JetStream's own limit backs up the worker's: {configured:?}"
    );
    assert_eq!(
        configured.backoff.first(),
        Some(&Duration::from_secs(30)),
        "the first redeliveries keep the ack wait: {configured:?}"
    );
    assert!(
        configured.backoff.last() > configured.backoff.first(),
        "later redeliveries back off: {configured:?}"
    );
    assert_eq!(configured.ack_wait, Duration::from_secs(30));
    worker.abort();
    let stopped = worker.await;
    assert!(matches!(stopped, Err(error) if error.is_cancelled()));
    Ok(())
}
