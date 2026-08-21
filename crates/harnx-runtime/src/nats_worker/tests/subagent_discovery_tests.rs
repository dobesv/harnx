use super::*;

fn write_package_agent(seeded: &SeededRemoteParentConfig, agent: &str, contents: &str) {
    let agents_dir = seeded
        .config_dir()
        .join("packages")
        .join("pantheon")
        .join("agents");
    std::fs::create_dir_all(&agents_dir).expect("create package agents directory");
    std::fs::write(agents_dir.join(format!("{agent}.md")), contents).expect("write package agent");
}

fn capture_selected_tools(
    captured_tools: Arc<AsyncMutex<Vec<String>>>,
) -> crate::agent_loop::AgentCallFn {
    Arc::new(move |_input, config, _abort| {
        let selected = {
            let config = config.read();
            let agent = config.agent.as_ref().expect("active package agent");
            config
                .select_tools(agent)
                .unwrap_or_default()
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>()
        };
        let captured_tools = Arc::clone(&captured_tools);
        Box::pin(async move {
            *captured_tools.lock().await = selected;
            Ok((
                "review complete".to_string(),
                None,
                vec![],
                crate::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

fn write_registration_race_agents(seeded: &SeededRemoteParentConfig) {
    write_package_agent(
        seeded,
        "aristarchus",
        "---\nuse_tools:\n  - zzz-reviewer_session_prompt\n---\nReview coordinator\n",
    );
    for index in 0..16 {
        write_package_agent(
            seeded,
            &format!("specialist-{index:02}"),
            "---\n---\nConfigured package specialist\n",
        );
    }
    write_package_agent(
        seeded,
        "zzz-reviewer",
        "---\n---\nConfigured final package specialist\n",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn package_agent_sees_bare_same_package_delegation_tool() {
    harnx_core::require_nextest();
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    for agent in ["aristarchus", "pytheas"] {
        write_package_agent(&seeded, agent, "---\n---\nConfigured package agent\n");
    }

    let captured = Arc::new(AsyncMutex::new(Vec::new()));
    let daemon = spawn_metis_worker_with_call_fn(&url, echoing_call_fn(captured));
    let client = async_nats::connect(&url)
        .await
        .expect("connect registry observer");
    let jetstream = async_nats::jetstream::new(client);
    let (instance_id, provider, registrations) = registered_agent_provider(
        &jetstream,
        &seeded.parent_config,
        &["pantheon/pytheas"],
        Some("pantheon"),
    )
    .await;

    let (key, registration) = registrations
        .iter()
        .find(|(_, registration)| {
            registration.package.as_deref() == Some("pantheon") && registration.server == "pytheas"
        })
        .expect("package agent registration exists");
    assert_eq!(key, &format!("{instance_id}.pantheon____pytheas"));
    let raw_tools: Vec<_> = registration
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(
        raw_tools,
        [
            "session_new",
            "session_prompt",
            "session_load",
            "session_cancel"
        ]
    );

    let declarations = provider.declarations_for_use_tools(Some("pytheas_session_prompt"));
    assert_eq!(declarations[0].name, "pytheas_session_prompt");
    let (result, _) = call_registered_agent(
        provider,
        "pytheas_session_prompt".to_string(),
        "assemble review context".to_string(),
        None,
    )
    .await;
    assert_eq!(
        result["response"],
        "stub remote reply over nats: assemble review context"
    );

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_package_agent_turn_waits_for_delegation_registrations() {
    harnx_core::require_nextest();
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    write_registration_race_agents(&seeded);

    let selected_tools = Arc::new(AsyncMutex::new(Vec::new()));
    let call_fn = capture_selected_tools(Arc::clone(&selected_tools));
    let client = async_nats::connect(&url)
        .await
        .expect("connect readiness observer");
    let mut readiness = client
        .subscribe(super::super::worker_ready_subject("local"))
        .await
        .expect("subscribe worker readiness");
    client.flush().await.expect("flush readiness subscription");
    let daemon = spawn_metis_worker_with_call_fn(&url, call_fn);
    tokio::time::timeout(Duration::from_secs(5), readiness.next())
        .await
        .expect("worker readiness timed out")
        .expect("worker readiness subscription closed");

    let session = NatsSession::new(
        crate::NatsSessionConfig {
            cluster: "local".to_string(),
            agent: "pantheon/aristarchus".to_string(),
            session_id: None,
            activation_route: crate::SessionActivationRoute::ClusterShared,
        },
        client.clone(),
        async_nats::jetstream::new(client),
        harnx_core::abort::create_abort_signal(),
    )
    .await
    .expect("create package parent session");
    // Keep a shorter test backstop than the production registration barrier so
    // a regression fails promptly instead of consuming the full 30 seconds.
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        session.run_turn("review this change", Arc::new(NoopEventSink), None),
    )
    .await
    .expect("first package turn timed out")
    .expect("first package turn failed");
    assert_eq!(result.response.as_deref(), Some("review complete"));
    assert_eq!(
        selected_tools.lock().await.as_slice(),
        ["zzz-reviewer_session_prompt"]
    );

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}
