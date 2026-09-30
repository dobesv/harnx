use super::*;
use anyhow::Context;

type CapturedAgent = (String, Option<f64>);

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
    NatsSessionLog::new_with_replicas(jetstream.clone(), &storage_key, 1)
        .append_event_async(&append_user_message_entry(message_id, message_id))
        .await?;
    publish_session_activate(jetstream, "local", &SessionActivate::new(storage_key), 1).await?;
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
    let _config_guard = EnvGuard::set(
        "HARNX_CONFIG_DIR",
        config_root.path().to_str().context("UTF-8 config path")?,
    );
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
async fn same_local_id_executes_with_each_agents_configuration() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let config_root = tempfile::tempdir()?;
    for agent in ["alpha", "beta"] {
        write_test_agent(config_root.path(), agent, agent)?;
    }
    let _config_guard = EnvGuard::set(
        "HARNX_CONFIG_DIR",
        config_root.path().to_str().context("UTF-8 config path")?,
    );
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
