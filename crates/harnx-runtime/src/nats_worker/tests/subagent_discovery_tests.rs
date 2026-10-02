use super::*;

pub(super) async fn wait_for_cluster_worker(config: &Config, cluster: &str) -> anyhow::Result<()> {
    let client = config.nats_client(cluster).await?;
    let mut readiness = client
        .subscribe(crate::nats_worker::worker_ready_subject(cluster))
        .await?;
    client.flush().await?;
    tokio::time::timeout(Duration::from_secs(10), readiness.next())
        .await
        .context("worker readiness timed out")?
        .context("worker readiness subscription closed")?;
    Ok(())
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

fn repeated_delegation_call_fn() -> crate::agent_loop::AgentCallFn {
    const FIRST_PROMPT: &str = "complete the first delegated task";
    const SECOND_PROMPT: &str = "complete the second delegated task";
    let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Arc::new(move |input, _config, _abort| {
        let call = call_count.fetch_add(1, Ordering::SeqCst);
        let prompt = input.text();
        Box::pin(async move {
            let (text, tools) = match call {
                0 => ("first delegation", vec![(FIRST_PROMPT, "first-call")]),
                1 if prompt == FIRST_PROMPT => ("first child complete", vec![]),
                2 => ("second delegation", vec![(SECOND_PROMPT, "second-call")]),
                3 if prompt == SECOND_PROMPT => ("second child complete", vec![]),
                4 => ("parent complete", vec![]),
                _ => panic!("unexpected model call {call} with prompt {prompt:?}"),
            };
            Ok((
                text.to_string(),
                None,
                tools
                    .into_iter()
                    .map(|(message, id)| {
                        ToolCall::new(
                            "metis_session_prompt".to_string(),
                            json!({ "message": message }),
                            Some(id.to_string()),
                            None,
                        )
                    })
                    .collect(),
                crate::client::CompletionTokenUsage::default(),
            ))
        })
    })
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
            initializer: crate::SessionInitializer::named(
                "pantheon/aristarchus",
                Default::default(),
            ),
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
        session.clone().with_external_admission().run_turn(
            "review this change",
            Arc::new(NoopEventSink),
            None,
        ),
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

#[test]
fn parent_can_delegate_again_after_nested_subagent_completes() {
    run_with_bounded_worker_stack(repeated_nested_delegation());
}

async fn repeated_nested_delegation() {
    harnx_core::require_nextest();
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let daemon = spawn_metis_worker_with_call_fn(&url, repeated_delegation_call_fn());
    let client = async_nats::connect(&url)
        .await
        .expect("connect parent session");
    let session = NatsSession::new(
        cluster_shared_session_config("local", crate::nats_worker::new_remote_session_id()),
        client.clone(),
        async_nats::jetstream::new(client),
        harnx_core::abort::create_abort_signal(),
    )
    .await
    .expect("create parent session");

    // Two complete nested turns cost about four seconds of transcript
    // projection I/O on an idle machine. This backstop only exists to turn a
    // wedged turn into a message instead of a hang, so it sits far above that:
    // a contended CI runner has been measured taking well over ten times the
    // idle cost for broker-backed work, and a backstop inside that range
    // reports a slow runner as a broken turn. Stay under nextest's own
    // terminate-after so the panic below is what surfaces.
    const PARENT_TURN_BACKSTOP: Duration = Duration::from_secs(180);
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        PARENT_TURN_BACKSTOP,
        session.clone().with_external_admission().run_turn(
            "delegate twice",
            Arc::new(NoopEventSink),
            None,
        ),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "parent turn did not finish within {}s (idle cost is ~4s, elapsed {:?}); \
             the second delegation never completed",
            PARENT_TURN_BACKSTOP.as_secs(),
            started.elapsed()
        )
    })
    .expect("parent turn failed");
    assert_eq!(
        result.response.as_deref(),
        Some("parent complete"),
        "parent error: {:?}",
        result.error
    );

    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_metadata_uses_worker_target_policy_and_package_patches_not_caller() {
    let _env_guard = env_lock().await;
    let Some((url, mut nats, _store_dir)) = spawn_test_nats().await else {
        return;
    };
    let mut seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    write_package_agent(
        &seeded,
        "finite",
        "---\nrun_limits:\n  timeout_secs: 40\n---\nFinite target\n",
    );
    write_package_agent(
        &seeded,
        "long",
        "---\nrun_limits:\n  timeout_secs: 2592000\n---\nLong finite target\n",
    );
    write_package_agent(
        &seeded,
        "inherit",
        "---\nrun_limits:\n  timeout_secs: 40\n---\nPatched inherited target\n",
    );
    std::fs::write(
        seeded.config_dir().join("packages/pantheon.patch.yaml"),
        "agents:\n  - 'if .name == \"finite\" then .run_limits.timeout_secs = 9 elif .name == \"inherit\" then .run_limits.timeout_secs = -1 else . end'\n",
    )
    .unwrap();
    // Caller policy intentionally conflicts with the worker's 24-hour fallback.
    seeded.parent_config.data.run_limits = serde_yaml::from_str("timeout_secs: 999").unwrap();
    let daemon = spawn_metis_worker_with_call_fn(
        &url,
        echoing_call_fn(Arc::new(AsyncMutex::new(Vec::new()))),
    );
    let js = async_nats::jetstream::new(async_nats::connect(&url).await.unwrap());
    let (_, provider, registrations) = registered_agent_provider(
        &js,
        &seeded.parent_config,
        &[
            "pantheon/finite",
            "pantheon/long",
            "pantheon/inherit",
            "metis",
        ],
        Some("pantheon"),
    )
    .await;
    for (name, expected) in [
        ("finite", "9 seconds"),
        ("long", "2592000 seconds"),
        ("inherit", "86400 seconds"),
        ("metis", "86400 seconds"),
    ] {
        let registration = &registrations
            .iter()
            .find(|(_, r)| r.server == name)
            .unwrap()
            .1;
        for tool in registration
            .tools
            .iter()
            .filter(|t| ["session_prompt", "session_new"].contains(&t.name.as_str()))
        {
            assert!(tool.description.contains(expected), "{}", tool.description);
            assert!(!tool.description.contains("999"));
            assert!(!tool.input_schema.to_string().contains("token_budget"));
        }
        let prompt = registration
            .tools
            .iter()
            .find(|t| t.name == "session_prompt")
            .unwrap();
        assert!(prompt
            .description
            .contains("Omitted, null, zero or negative inherits target policy"));
        assert!(prompt
            .description
            .contains("fallback: 86400 seconds / 24 hours"));
        assert!(prompt
            .description
            .contains("inherited deadlines can shorten"));
        assert!(prompt.input_schema["properties"]["timeout_secs"]
            .get("default")
            .is_none());
    }
    for timeout in [
        None,
        Some(json!(null)),
        Some(json!(0)),
        Some(json!(-1)),
        Some(json!(i64::MIN)),
    ] {
        let mut args = json!({"message": "verify target policy"});
        if let Some(value) = timeout {
            args["timeout_secs"] = value;
        }
        let result = provider
            .call_tool_with_id(
                "finite_session_prompt",
                args,
                None,
                &harnx_core::abort::create_abort_signal(),
            )
            .await
            .unwrap_or_else(|error| match error {
                harnx_core::tool::ToolError::Recoverable(error)
                | harnx_core::tool::ToolError::Fatal(error) => {
                    panic!("numeric inherited call failed: {error:#}")
                }
            })
            .value;
        let storage = harnx_core::session_identity::session_key(
            Some("pantheon/finite"),
            result["session_id"].as_str().unwrap(),
        );
        let entries = NatsSessionLog::new(js.clone(), &storage)
            .load_events_latest_async()
            .await
            .unwrap();
        let prompt_id = entries
            .iter()
            .find_map(|(_, e)| match e {
                SessionLogEntry::Message {
                    id,
                    role: harnx_core::message::MessageRole::User,
                    ..
                } => id.clone(),
                _ => None,
            })
            .unwrap();
        let record = SessionMetadataStore::ensure(&js, 1)
            .await
            .unwrap()
            .get_invocation_limits(&storage, &prompt_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (record.deadline.unwrap() - record.admitted_at).num_seconds(),
            9
        );
        assert_eq!(
            record.policy_source,
            crate::nats_session_metadata::RunLimitsPolicySource::TargetAgent {
                agent_name: "pantheon/finite".into()
            }
        );
    }
    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
}
