use super::*;

#[test]
fn missing_worker_reports_full_error_after_prompt_is_saved() {
    harnx_core::require_nextest();
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let missing_worker = std::env::temp_dir()
        .join(format!("missing-worker-{}", uuid::Uuid::new_v4()))
        .join("harnx-worker");
    // SAFETY: nextest runs this test in its own process, and we set the override
    // before creating the async runtime or starting any broker/worker tasks.
    // The runtime is dropped before TestConfigSandbox restores the environment.
    unsafe { std::env::set_var("HARNX_WORKER_BIN", &missing_worker) };
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(assert_missing_worker_error(&sandbox, &missing_worker));
}

async fn assert_missing_worker_error(
    sandbox: &TestConfigSandbox,
    missing_worker: &std::path::Path,
) {
    if !crate::test_support::ensure_test_nats().await {
        return;
    }
    let config = sandbox.config();
    let session_id = format!("missing-worker-session-{}", uuid::Uuid::new_v4());
    let registry = SessionRegistry::new(config.clone());
    let handle = registry.get_or_spawn(key("plain", &session_id));
    let mut events = subscribe(&handle).await.events;

    assert!(matches!(
        prompt(&handle, "keep this prompt").await,
        PromptResult::Accepted { .. }
    ));
    let error = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await.expect("worker startup event") {
                Event::RunError(error) => break error.message,
                Event::RunFinished(_) => panic!("missing worker must fail the run"),
                _ => {}
            }
        }
    })
    .await
    .expect("missing worker must not silently hang");
    assert!(
        error.contains("failed to ensure local NATS worker"),
        "{error}"
    );
    assert!(error.contains("HARNX_WORKER_BIN"), "{error}");
    assert!(
        error.contains(&missing_worker.display().to_string()),
        "{error}"
    );

    let info = get_info(&handle).await;
    assert_eq!(info.state, SessionState::Idle);
    let (session, _) = crate::load_nats_session(
        &config,
        &crate::session_actor::ResolvedAgentTarget::local("plain"),
        &session_id,
    )
    .await
    .expect("load saved prompt");
    assert_eq!(session.messages.len(), 1);
    assert!(matches!(
        &session.messages[0].content,
        harnx_core::message::MessageContent::Text(text) if text == "keep this prompt"
    ));
}
