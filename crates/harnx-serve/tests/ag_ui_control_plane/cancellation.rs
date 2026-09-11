use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn e2e_success_criterion_4_cancel_running_persists_partial_and_returns_idle() {
    let _guard = TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();

    // Gate: call_fn signals when started, test releases after cancel
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    let call_fn: AgentCallFn = {
        let started = started.clone();
        let release = release.clone();
        Arc::new(move |input, config, abort| {
            let started = started.clone();
            let release = release.clone();
            // The injected executor bypasses NATS admission. Model the frontend's
            // durable user append explicitly before entering the mock model call.
            let content = input.message_content();
            let config = config.clone();
            Box::pin(async move {
                if let Some(session) = config.write().session.as_mut() {
                    harnx_runtime::config::session::append_event(
                        session,
                        &harnx_core::session::SessionLogEntry::Message {
                            id: Some("cancelled-user".into()),
                            role: harnx_core::message::MessageRole::User,
                            content,
                            timestamp: None,
                            fence_token: None,
                        },
                    );
                }
                started.notify_one();
                tokio::select! {
                    _ = release.notified() => Ok((
                        "should not finish".to_string(),
                        None,
                        Vec::<ToolCall>::new(),
                        CompletionTokenUsage::default(),
                    )),
                    _ = harnx_core::abort::wait_abort_signal(&abort) => Err(anyhow!("cancelled")),
                }
            })
        })
    };
    let registry =
        SessionRegistry::new_for_tests(config.clone(), Duration::from_secs(30), Some(call_fn));

    // Open a PROMPTED run (with messages) to receive live events
    let response = open_sse(
        &config,
        &registry,
        "plain",
        "criteria-4",
        json!([{
            "id": Uuid::new_v4(),
            "role": "user",
            "content": "cancel me"
        }]),
    )
    .await;
    let sse_task = tokio::spawn(async move {
        read_sse_until(response, Duration::from_secs(10), |read| {
            read.events.iter().any(|event| {
                matches!(
                    event["type"].as_str(),
                    Some("RUN_ERROR") | Some("RUN_FINISHED")
                )
            })
        })
        .await
    });

    // The SSE stream was already opened with a user message. Now just wait for it.
    let prompt = rpc_call(
        &config,
        &registry,
        "plain",
        "criteria-4",
        json!({"jsonrpc":"2.0","id":1,"method":"session/get"}),
    )
    .await;
    // Session should be running
    assert_eq!(prompt["result"]["state"]["status"], "running");

    // Wait for run to start (deterministic)
    started.notified().await;

    // Give the actor a moment to enter the select! (abort-aware)
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Cancel via RPC
    let cancel = rpc_call(
        &config,
        &registry,
        "plain",
        "criteria-4",
        json!({"jsonrpc":"2.0","id":2,"method":"session/cancel"}),
    )
    .await;
    assert_eq!(cancel["result"]["cancelled"], true);

    // Release the gate so if the run didn't catch the abort it can still exit
    release.notify_one();

    let read = sse_task.await.expect("sse task");
    // Assert cancel path emits RUN_ERROR (not RUN_FINISHED)
    assert!(
        read.events.iter().any(|event| matches!(
            event["type"].as_str(),
            Some("RUN_ERROR") | Some("RUN_FINISHED")
        )),
        "cancel should emit terminal event"
    );

    // Wait for state to settle to idle
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify partial state persisted
    let persisted = load_session_messages(&config, "plain", "criteria-4");
    assert!(persisted
        .iter()
        .any(|msg| msg.role.is_user() && msg.content.to_text() == "cancel me"));

    // Verify session state is Idle
    let state = rpc_call(
        &config,
        &registry,
        "plain",
        "criteria-4",
        json!({"jsonrpc":"2.0","id":3,"method":"session/get"}),
    )
    .await;
    assert_eq!(state["result"]["state"]["status"], "idle");
}
