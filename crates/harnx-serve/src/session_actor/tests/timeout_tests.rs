use super::*;

fn actor_for_timeout(config: Config, session: &str) -> SessionActor {
    let call_fn: AgentCallFn = Arc::new(|_, _, _| {
        Box::pin(async { Ok(("done".into(), None, vec![], Default::default())) })
    });
    make_session_actor(
        key("plain", session),
        Default::default(),
        Duration::from_secs(30),
        SessionActorConfig {
            base_config: config,
            call_fn: Some(call_fn),
            local_worker: Arc::new(Mutex::new(None)),
        },
    )
    .0
}

#[tokio::test]
async fn timeout_clamp_resolves_agent_global_and_default_policy() {
    let sandbox = TestConfigSandbox::new();
    for (index, (agent, global, configured)) in [
        ("run_limits:\n  timeout_secs: 30", json!(60), 30),
        ("run_limits:\n  timeout_secs: 90", json!(60), 90),
        ("", json!(60), 60),
        ("run_limits:\n  timeout_secs: 0", json!(60), 60),
        ("run_limits:\n  timeout_secs: -1", json!(60), 60),
        ("run_limits:\n  timeout_secs: null", json!(60), 60),
        ("", json!(null), 86400),
    ]
    .into_iter()
    .enumerate()
    {
        sandbox.write_agent_with_front_matter("plain", agent, "You are plain.");
        let mut config = sandbox.config();
        config.data.run_limits = serde_json::from_value(json!({"timeout_secs": global})).unwrap();
        let actor = actor_for_timeout(config, &format!("clamp-{index}"));
        for (request, expected) in [
            (None, None),
            (Some(0), None),
            (Some(configured - 1), Some(configured - 1)),
            (Some(configured), None),
            (Some(configured + 1), None),
            (Some(u64::MAX), None),
        ] {
            assert_eq!(
                actor.clamp_timeout_override(request).await,
                expected,
                "agent={agent:?} global={global} request={request:?}"
            );
        }
    }
}

#[tokio::test]
async fn injected_executor_timeout_admission_cannot_extend_policy() {
    let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent_with_front_matter(
        "plain",
        "run_limits:\n  timeout_secs: 30",
        "You are plain.",
    );
    for (index, (request, seconds)) in [
        (None, 30),
        (Some(0), 30),
        (Some(10), 10),
        (Some(30), 30),
        (Some(60), 30),
    ]
    .into_iter()
    .enumerate()
    {
        let mut actor = actor_for_timeout(sandbox.config(), &format!("executor-clamp-{index}"));
        let reap = tokio::time::sleep(Duration::from_secs(60));
        tokio::pin!(reap);
        actor
            .start_run(
                "hello".into(),
                SessionPromptOptions {
                    timeout_secs: request,
                    ..Default::default()
                },
                &mut reap,
            )
            .await;
        let done = tokio::time::timeout(Duration::from_secs(10), actor.run_done_rx.recv())
            .await
            .unwrap()
            .unwrap();
        done.result.expect("injected turn completed");
        let limits = done.limits.expect("injected executor admission");
        assert_eq!(
            limits
                .deadline
                .unwrap()
                .signed_duration_since(limits.admitted_at)
                .num_seconds(),
            seconds
        );
    }
}
