use super::*;

fn registration_agent(registration: &harnx_toolset::Registration) -> String {
    registration.package.as_ref().map_or_else(
        || registration.server.clone(),
        |package| format!("{package}/{}", registration.server),
    )
}

pub(super) async fn registered_agent_provider(
    jetstream: &async_nats::jetstream::Context,
    config: &Config,
    agents: &[&str],
    active_package: Option<&str>,
) -> (
    String,
    Arc<crate::nats_tool_provider::NatsToolProvider>,
    Vec<(String, harnx_toolset::Registration)>,
) {
    let (instance_id, registrations) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(registry) = jetstream
                .get_key_value(harnx_toolset_server::TOOL_REGISTRY_BUCKET)
                .await
            {
                let mut keys = registry.keys().await.expect("list registry keys");
                let mut registrations = Vec::new();
                while let Some(key) = keys.next().await {
                    let key = key.expect("read registry key");
                    let Some(value) = registry.get(&key).await.expect("read registration") else {
                        continue;
                    };
                    let registration = serde_json::from_slice(&value).expect("decode registration");
                    registrations.push((key, registration));
                }
                if agents.iter().all(|agent| {
                    registrations
                        .iter()
                        .any(|(_, registration)| registration_agent(registration) == *agent)
                }) {
                    let agent = agents.first().expect("at least one requested agent");
                    let (key, registration) = registrations
                        .iter()
                        .find(|(_, registration)| registration_agent(registration) == *agent)
                        .expect("requested agent registration exists");
                    let identity =
                        crate::server_identity::ServerIdentity::identity_token(registration);
                    let instance_id = key
                        .strip_suffix(&format!(".{identity}"))
                        .expect("registry key uses {instance}.{identity_token}")
                        .to_string();
                    break (instance_id, registrations);
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("worker did not register configured agents");
    let provider = crate::nats_tool_provider::NatsToolProvider::discover(
        config,
        harnx_core::instance::ServerScope::from_string(instance_id.clone()),
        crate::nats_tool_provider::NatsInFlightCalls::default(),
        active_package,
    )
    .await
    .expect("parent discovers configured sub-agent toolsets");
    (instance_id, Arc::new(provider), registrations)
}

pub(super) async fn call_registered_agent(
    provider: Arc<crate::nats_tool_provider::NatsToolProvider>,
    tool: String,
    message: String,
    early_event: Option<(&mut async_nats::Subscriber, &str)>,
) -> (serde_json::Value, Option<String>) {
    let prompt_call = tokio::spawn(async move {
        provider
            .call_tool(
                &tool,
                json!({ "message": message }),
                &harnx_core::abort::create_abort_signal(),
            )
            .await
    });
    let child_session_id = if let Some((parent_events, expected_agent)) = early_event {
        let (agent, session_id) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let message = parent_events
                    .next()
                    .await
                    .expect("parent event stream closed");
                let envelope =
                    crate::nats_event_sink::AdvisoryEnvelope::from_bytes(&message.payload)
                        .expect("decode parent advisory");
                if let AgentEvent::SubAgent { source, event } = envelope.event {
                    if let AgentEvent::Turn(harnx_core::event::TurnEvent::SubAgentStarted {
                        agent,
                        session_id,
                    }) = *event
                    {
                        assert_eq!(source.agent, agent);
                        assert_eq!(source.session_id.as_deref(), Some(session_id.as_str()));
                        break (agent, session_id);
                    }
                }
            }
        })
        .await
        .expect("parent did not receive early SubAgentStarted");
        assert_eq!(agent, expected_agent);
        assert!(
            !prompt_call.is_finished(),
            "SubAgentStarted must arrive before final tool result"
        );
        Some(session_id)
    } else {
        None
    };
    let result = prompt_call
        .await
        .expect("join prompt tool call")
        .unwrap_or_else(|error| match error {
            harnx_core::tool::ToolError::Recoverable(error)
            | harnx_core::tool::ToolError::Fatal(error) => {
                panic!("registered agent prompt failed: {error:#}")
            }
        });
    (result, child_session_id)
}

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

    let thin = ThinClientSession::new(
        crate::ThinClientConfig {
            cluster: "local".to_string(),
            agent: "pantheon/aristarchus".to_string(),
            session_id: None,
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
        thin.run_turn("review this change", Arc::new(NoopEventSink), None),
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
