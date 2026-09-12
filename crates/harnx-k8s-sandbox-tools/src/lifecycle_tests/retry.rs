use super::*;

#[tokio::test(start_paused = true)]
async fn transient_reads_use_exact_attempt_budget_and_preserve_last_cause() {
    let api = Arc::new(MockApi::default());
    api.state.lock().get_errors_remaining = usize::MAX;
    let manager = test_manager(api.clone()).with_backoff(
        BackoffConfig::new(Duration::from_secs(1), Duration::from_secs(8), 3)
            .with_sampler(Arc::new(ZeroJitter)),
    );

    let error = manager
        .ensure_active("exhausted", &CancellationToken::new())
        .await
        .unwrap_err();
    let terminal = error.downcast_ref::<TerminalError>().unwrap();

    assert_eq!(terminal.end_reason, EndReason::AttemptsExhausted);
    assert_eq!(terminal.attempts, 3);
    assert!(terminal.to_string().contains("transient Kubernetes error"));
    assert_eq!(api.state.lock().get_calls, 3);
}

#[tokio::test(start_paused = true)]
async fn permanent_kubernetes_error_fails_after_one_attempt() {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::set_global_recorder(recorder).expect("test process has no metrics recorder");
    let api = Arc::new(MockApi::default());
    api.state
        .lock()
        .scripted_get_errors
        .push_back(typed_api_error(403, 0));

    let error = test_manager(api.clone())
        .ensure_active("forbidden", &CancellationToken::new())
        .await
        .unwrap_err();
    let terminal = error.downcast_ref::<TerminalError>().unwrap();
    let matching_metrics = snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(key, _, _, value)| {
            key.key().name() == "harnx_sandbox_gateway_operation_total"
                && key
                    .key()
                    .labels()
                    .any(|label| label.key() == "operation" && label.value() == "ensure_active")
                && key
                    .key()
                    .labels()
                    .any(|label| label.key() == "outcome" && label.value() == "permanent_error")
                && *value == DebugValue::Counter(1)
        })
        .count();

    assert_eq!(
        (terminal.end_reason, terminal.last_kind, terminal.attempts),
        (EndReason::Failed, FailureKind::Permanent, 1)
    );
    assert_eq!(api.state.lock().get_calls, 1);
    assert_eq!(matching_metrics, 1);
}

#[tokio::test(start_paused = true)]
async fn kubernetes_retry_after_sets_minimum_backoff_delay() {
    let api = Arc::new(MockApi::default());
    api.insert(record("rate-limited", Some(1), None, Utc::now()));
    api.state
        .lock()
        .scripted_get_errors
        .push_back(typed_api_error(429, 7));
    let manager = SandboxManager::new(
        api.clone(),
        SandboxManagerConfig {
            activation_timeout: Duration::from_secs(20),
            ..SandboxManagerConfig::default()
        },
    )
    .with_backoff(
        BackoffConfig::new(Duration::from_secs(1), Duration::from_secs(1), 2)
            .with_sampler(Arc::new(ZeroJitter)),
    );
    let started = tokio::time::Instant::now();

    let ip = manager
        .ensure_active("rate-limited", &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(ip, "10.0.0.8");
    assert_eq!(api.state.lock().get_calls, 2);
    assert_eq!(started.elapsed(), Duration::from_secs(7));
}
