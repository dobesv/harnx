use super::*;
use anyhow::Context;

fn write_test_agent(config_dir: &Path, agent_name: &str, body: &str) -> Result<()> {
    let agents_dir = config_dir.join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    std::fs::write(agents_dir.join(format!("{agent_name}.md")), body)?;
    Ok(())
}

struct SeedActivation<'a> {
    session_id: &'a str,
    initializer: SessionInitializer,
    message_id: &'a str,
}

async fn seed_and_activate(
    jetstream: &async_nats::jetstream::Context,
    store: &SessionMetadataStore,
    seed: SeedActivation<'_>,
) -> Result<()> {
    let SeedActivation {
        session_id,
        initializer,
        message_id,
    } = seed;
    let storage_key = initializer.session_key(session_id);
    store
        .create(&SessionMetadata::new(session_id, initializer))
        .await?;
    crate::worker::append_admitted_fixture_user(
        &NatsSessionLog::new_with_replicas(jetstream.clone(), &storage_key, 1),
        message_id,
        message_id,
    )
    .await?;
    publish_session_activate(jetstream, "local", &SessionActivate::new(storage_key), 1).await?;
    Ok(())
}
fn failing_call_fn(counter: Arc<AtomicUsize>) -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        let counter = Arc::clone(&counter);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("model turn failure")
        })
    })
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
async fn wait_for_error(log: &NatsSessionLog) -> Result<String> {
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            if let Some(message) =
                log.load_events_async()
                    .await?
                    .into_iter()
                    .find_map(|(_, entry)| match entry {
                        SessionLogEntry::Error { message, .. } => Some(message),
                        _ => None,
                    })
            {
                return Ok::<_, anyhow::Error>(message);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}

async fn assert_duplicate_activation_is_covered(
    jetstream: &async_nats::jetstream::Context,
    log: &NatsSessionLog,
    session_key: &str,
) -> Result<()> {
    let stream = jetstream.get_stream("WORK_NOTIFY_local").await?;
    let consumer: async_nats::jetstream::consumer::PullConsumer = stream
        .get_consumer("workers")
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    wait_for_terminal_activation(&consumer, &stream, 1).await?;
    let delivered_before_duplicate = consumer.get_info().await?.delivered.consumer_sequence;
    publish_session_activate(jetstream, "local", &SessionActivate::new(session_key), 1).await?;
    wait_for_terminal_activation(&consumer, &stream, delivered_before_duplicate + 1).await?;
    assert_eq!(
        log.load_events_async()
            .await?
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::Error { .. }))
            .count(),
        1,
        "covered activation must not rerun an already failed turn"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_named_agent_fails_durably_without_calling_the_model() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let config_root = tempfile::tempdir()?;
    let _config_guard = EnvGuard::set(
        "HARNX_CONFIG_DIR",
        config_root.path().to_str().context("UTF-8 config path")?,
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "worker-missing-agent",
        counting_stub_call_fn(Arc::clone(&calls)),
    )
    .await?;
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let session_id = "missing-named-agent";
    let session = NatsSession::new(
        NatsSessionConfig {
            cluster: "local".to_string(),
            initializer: SessionInitializer::named("does-not-exist", Default::default()),
            session_id: Some(session_id.to_string()),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        client,
        jetstream.clone(),
        create_abort_signal(),
    )
    .await?;
    let outcome = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        session.clone().with_external_admission().run_turn(
            "missing-user",
            Arc::new(NullSink),
            None,
        ),
    )
    .await??;
    let caller_error = outcome
        .error
        .context("caller did not receive worker failure")?;
    assert!(
        caller_error.contains("does-not-exist"),
        "unexpected caller error: {caller_error}"
    );

    let session_key = harnx_core::session_identity::session_key(Some("does-not-exist"), session_id);
    let log = NatsSessionLog::new_with_replicas(jetstream.clone(), &session_key, 1);
    let entries = log.load_events_async().await?;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::Error { .. })));

    assert_duplicate_activation_is_covered(&jetstream, &log, &session_key).await?;
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turn_failure_is_recorded_and_returned_without_redelivery() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "worker-turn-failure",
        failing_call_fn(Arc::clone(&calls)),
    )
    .await?;
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let session_id = "immediate-turn-failure";
    let session = NatsSession::new(
        NatsSessionConfig {
            cluster: "local".to_string(),
            initializer: SessionInitializer::inline(
                "failing test agent",
                Default::default(),
                SessionOverrides::default(),
            ),
            session_id: Some(session_id.to_string()),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        client,
        jetstream.clone(),
        create_abort_signal(),
    )
    .await?;

    let outcome = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        session
            .clone()
            .with_external_admission()
            .run_turn("fail once", Arc::new(NullSink), None),
    )
    .await??;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("model turn failure")),
        "caller did not receive turn failure: {outcome:?}"
    );
    let errors = NatsSessionLog::new_with_replicas(jetstream.clone(), storage_key(session_id), 1)
        .load_events_async()
        .await?
        .into_iter()
        .filter(|(_, entry)| matches!(entry, SessionLogEntry::Error { .. }))
        .count();
    assert_eq!(errors, 1);

    let stream = jetstream.get_stream("WORK_NOTIFY_local").await?;
    let consumer: async_nats::jetstream::consumer::PullConsumer = stream
        .get_consumer("workers")
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    assert_eq!(consumer.get_info().await?.delivered.consumer_sequence, 1);
    assert_eq!(stream.get_info().await?.state.messages, 0);

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn named_local_agent_without_model_fails_with_clear_durable_error() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let config_root = tempfile::tempdir()?;
    write_test_agent(config_root.path(), "model-less", "run without a model")?;
    let _config_guard = EnvGuard::set(
        "HARNX_CONFIG_DIR",
        config_root.path().to_str().context("UTF-8 config path")?,
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let config = local_nats_runtime_config(server.url());
    config.write().model = Default::default();
    let daemon = spawn_worker_daemon_with_call_fn(
        config,
        "worker-model-less-agent",
        counting_stub_call_fn(Arc::clone(&calls)),
    )
    .await?;
    let jetstream = local_test_nats(server.url()).await?;
    let session_id = "model-less-local-agent";
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    seed_and_activate(
        &jetstream,
        &store,
        SeedActivation {
            session_id,
            initializer: SessionInitializer::named("model-less", Default::default()),
            message_id: "model-less-user",
        },
    )
    .await?;
    let log = NatsSessionLog::new_with_replicas(
        jetstream,
        harnx_core::session_identity::session_key(Some("model-less"), session_id),
        1,
    );

    let error_message = wait_for_error(&log).await?;

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        error_message.contains("no chat model configured for local agent 'model-less'")
            && error_message.contains("configure a client/model or route to a remote worker"),
        "unexpected worker error: {error_message}"
    );
    assert!(!error_message.contains("Invalid model ''"));
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

/// Nothing answers the session's control subject: the listener a failed
/// attempt subscribed there was stopped with it.
async fn assert_no_control_listener(client: &async_nats::Client, session_key: &str) -> Result<()> {
    let hint = harnx_runtime::ControlCommand::Compact {
        compaction_id: "listener-probe".into(),
    };
    poll_until(async || {
        Ok(harnx_runtime::nats_worker::request_control_command(
            client,
            session_key,
            &hint,
            Duration::from_secs(1),
        )
        .await
        .is_err())
    })
    .await
    .context("a listener from the failed attempt still answers the session's control subject")
}

/// A prompt with no durable run admission can never run. Every prompt from
/// before run admissions existed is one, including the one each session that
/// was mid-turn during the upgrade resumes. The worker used to refuse it and
/// hand the activation back, and JetStream redelivered it forever. Now a
/// second delivery confirms the refusal, records why on the session and
/// terminates the activation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unadmitted_prompt_fails_its_session_on_the_second_delivery() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "worker-unadmitted-prompt",
        counting_stub_call_fn(Arc::clone(&calls)),
    )
    .await?;
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let session_id = "unadmitted-prompt";
    seed_session_metadata(&jetstream, session_id).await?;
    let session_key = storage_key(session_id);
    let log = NatsSessionLog::new_with_replicas(jetstream.clone(), &session_key, 1);
    let prompt_seq = log
        .append_event_async(&SessionLogEntry::Message {
            id: Some("before-admissions".into()),
            role: MessageRole::User,
            content: harnx_core::message::MessageContent::Text("resume me".into()),
            timestamp: None,
            fence_token: None,
        })
        .await?;
    publish_session_activate(
        &jetstream,
        "local",
        &SessionActivate::new(&session_key).with_requested_seq(prompt_seq),
        1,
    )
    .await?;

    let error = wait_for_error(&log).await?;
    assert!(
        error.contains("refusing executable prompt without durable run admission")
            && error.contains("send it again"),
        "unexpected worker error: {error}"
    );
    let stream = jetstream.get_stream("WORK_NOTIFY_local").await?;
    let consumer: async_nats::jetstream::consumer::PullConsumer = stream
        .get_consumer("workers")
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    wait_for_terminal_activation(&consumer, &stream, 2).await?;
    assert_eq!(
        consumer.get_info().await?.delivered.consumer_sequence,
        2,
        "a refusal is settled on the delivery that confirms it"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_no_control_listener(&client, &session_key).await?;
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_shared_activation_is_terminated() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let worker = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "malformed-worker",
        counting_stub_call_fn(Arc::clone(&calls)),
    )
    .await?;
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    jetstream
        .publish(
            harnx_runtime::nats_worker::notify_subject("local"),
            bytes::Bytes::from_static(b"{not-json"),
        )
        .await?
        .await?;
    let stream = jetstream.get_stream("WORK_NOTIFY_local").await?;
    let consumer: async_nats::jetstream::consumer::PullConsumer = stream
        .get_consumer("workers")
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    wait_for_terminal_activation(&consumer, &stream, 1).await?;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    worker.abort();
    let stopped = worker.await;
    assert!(matches!(stopped, Err(error) if error.is_cancelled()));
    Ok(())
}
