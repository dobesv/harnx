use super::*;
use std::path::Path;

type CapturedAgent = (String, Option<f64>);

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set_path(key: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

fn write_test_agent(config_dir: &Path, agent_name: &str, body: &str) -> Result<()> {
    let agents_dir = config_dir.join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    std::fs::write(agents_dir.join(format!("{agent_name}.md")), body)?;
    Ok(())
}

fn capture_agent_call(
    captured: Arc<AsyncMutex<Vec<CapturedAgent>>>,
) -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |input, _config, _abort| {
        let captured = Arc::clone(&captured);
        let instructions = input.agent().instructions_template().to_string();
        let temperature = input.agent().temperature();
        Box::pin(async move {
            captured.lock().await.push((instructions, temperature));
            Ok((
                "done".to_string(),
                None,
                Vec::new(),
                CompletionTokenUsage::default(),
            ))
        })
    })
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
    NatsSessionLog::new_with_replicas(jetstream.clone(), &storage_key, 1)
        .append_event_async(&append_user_message_entry(message_id, message_id))
        .await?;
    publish_session_activate(jetstream, "local", &SessionActivate::new(storage_key), 1).await?;
    Ok(())
}

fn assert_agent_versions(captured: &[CapturedAgent]) {
    assert!(captured
        .iter()
        .any(|(instructions, temperature)| instructions == "version one" && temperature.is_none()));
    assert!(captured.iter().any(|(instructions, temperature)| {
        instructions == "version two" && *temperature == Some(0.42)
    }));
    assert!(captured
        .iter()
        .any(|(instructions, _)| instructions == "stored inline instructions"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn named_agents_reload_each_activation_and_inline_sessions_use_stored_prompt() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let config_root = tempfile::tempdir()?;
    write_test_agent(config_root.path(), "reload-agent", "version one")?;
    let _config_guard = EnvVarGuard::set_path("HARNX_CONFIG_DIR", config_root.path());
    let captured = Arc::new(AsyncMutex::new(Vec::<CapturedAgent>::new()));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "worker-agent-reload",
        capture_agent_call(Arc::clone(&captured)),
    )
    .await?;
    let jetstream = local_test_nats(server.url()).await?;
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;

    seed_and_activate(
        &jetstream,
        &store,
        SeedActivation {
            session_id: "named-agent-version-one",
            initializer: SessionInitializer::named("reload-agent", Default::default()),
            message_id: "first-user",
        },
    )
    .await?;
    wait_until(CI_SAFE_TIMEOUT, || {
        captured.try_lock().is_ok_and(|values| !values.is_empty())
    })
    .await?;

    write_test_agent(config_root.path(), "reload-agent", "version two")?;
    let mut second = SessionInitializer::named("reload-agent", Default::default());
    second.overrides.temperature = Some(0.42);
    seed_and_activate(
        &jetstream,
        &store,
        SeedActivation {
            session_id: "named-agent-version-two",
            initializer: second,
            message_id: "second-user",
        },
    )
    .await?;
    seed_and_activate(
        &jetstream,
        &store,
        SeedActivation {
            session_id: "inline-agent-prompt",
            initializer: SessionInitializer::inline(
                "stored inline instructions",
                Default::default(),
                SessionOverrides::default(),
            ),
            message_id: "inline-user",
        },
    )
    .await?;

    wait_until(CI_SAFE_TIMEOUT, || {
        captured.try_lock().is_ok_and(|values| values.len() >= 3)
    })
    .await?;
    assert_agent_versions(&captured.lock().await);
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_named_agent_fails_durably_without_calling_the_model() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let config_root = tempfile::tempdir()?;
    let _config_guard = EnvVarGuard::set_path("HARNX_CONFIG_DIR", config_root.path());
    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "worker-missing-agent",
        counting_stub_call_fn(Arc::clone(&calls)),
    )
    .await?;
    let jetstream = local_test_nats(server.url()).await?;
    let session_id = "missing-named-agent";
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    seed_and_activate(
        &jetstream,
        &store,
        SeedActivation {
            session_id,
            initializer: SessionInitializer::named("does-not-exist", Default::default()),
            message_id: "missing-user",
        },
    )
    .await?;
    let log = NatsSessionLog::new_with_replicas(
        jetstream,
        harnx_core::session_identity::session_key(Some("does-not-exist"), session_id),
        1,
    );

    let entries = tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            let entries = log.load_events_async().await?;
            if entries.iter().any(|(_, entry)| {
                matches!(entry, SessionLogEntry::Error { message, .. } if message.contains("does-not-exist"))
            }) {
                return Ok::<_, anyhow::Error>(entries);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::Error { .. })));
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
    let _config_guard = EnvVarGuard::set_path("HARNX_CONFIG_DIR", config_root.path());
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

    let error_message = tokio::time::timeout(CI_SAFE_TIMEOUT, async {
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
    .await??;

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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_local_id_executes_with_each_agents_configuration() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let config_root = tempfile::tempdir()?;
    for agent in ["alpha", "beta"] {
        write_test_agent(config_root.path(), agent, agent)?;
    }
    let _config_guard = EnvVarGuard::set_path("HARNX_CONFIG_DIR", config_root.path());
    let captured = Arc::new(AsyncMutex::new(Vec::<CapturedAgent>::new()));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "worker-agent-isolation",
        capture_agent_call(Arc::clone(&captured)),
    )
    .await?;
    let js = local_test_nats(server.url()).await?;
    let store = SessionMetadataStore::ensure(&js, 1).await?;
    for agent in ["alpha", "beta"] {
        seed_and_activate(
            &js,
            &store,
            SeedActivation {
                session_id: "review-12345",
                initializer: SessionInitializer::named(agent, Default::default()),
                message_id: agent,
            },
        )
        .await?;
    }
    wait_until(CI_SAFE_TIMEOUT, || {
        captured.try_lock().is_ok_and(|values| values.len() == 2)
    })
    .await?;
    let mut instructions: Vec<_> = captured
        .lock()
        .await
        .iter()
        .map(|(text, _)| text.clone())
        .collect();
    instructions.sort();
    assert_eq!(instructions, ["alpha", "beta"]);
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}
