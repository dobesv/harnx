use super::*;

#[tokio::test]
async fn ensure_active_wakes_extends_and_records_activity() {
    let api = Arc::new(MockApi::default());
    api.insert(record(
        "claim-2",
        Some(0),
        Some(Utc::now() + chrono::Duration::minutes(10)),
        Utc::now() - chrono::Duration::hours(1),
    ));

    let ip = test_manager(api.clone())
        .ensure_active("claim-2", &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(ip, "10.0.0.8");
    let state = api.state.lock();
    assert_eq!(state.replica_updates, [("claim-2".to_string(), 1)]);
    assert_eq!(state.shutdown_updates, ["claim-2"]);
    // Two bumps: pre-wake (before waiting) + post-ready (after pod is ready)
    assert_eq!(state.activity_updates, ["claim-2", "claim-2"]);
}

#[tokio::test]
async fn ensure_active_waits_for_a_pod_ip_after_readiness() {
    let api = Arc::new(MockApi::default());
    let mut without_ip = record("claim-ip", Some(1), None, Utc::now());
    without_ip.pod_ips.clear();
    let with_ip = record("claim-ip", Some(1), None, Utc::now());
    api.state
        .lock()
        .scripted_gets
        .extend([Some(without_ip), Some(with_ip)]);

    let ip = test_manager(api)
        .ensure_active("claim-ip", &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(ip, "10.0.0.8");
}

#[tokio::test]
async fn ensure_active_retries_transient_errors_during_each_wait_phase() {
    let api = Arc::new(MockApi::default());
    let pending = pending_record("claim-retry");
    let mut ready_without_ip = record("claim-retry", Some(1), None, Utc::now());
    ready_without_ip.pod_ips.clear();
    let ready_with_ip = record("claim-retry", Some(1), None, Utc::now());
    {
        let mut state = api.state.lock();
        state.get_error_calls.extend([1, 3, 5]);
        state
            .scripted_gets
            .extend([Some(pending), Some(ready_without_ip), Some(ready_with_ip)]);
    }

    let ip = test_manager(api.clone())
        .ensure_active("claim-retry", &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(ip, "10.0.0.8");
    assert_eq!(api.state.lock().get_calls, 6);
}

#[tokio::test(start_paused = true)]
async fn ensure_active_reports_a_missing_pod_ip_after_its_budget() {
    let api = Arc::new(MockApi::default());
    let mut without_ip = record("claim-no-ip", Some(1), None, Utc::now());
    without_ip.pod_ips.clear();
    api.insert(without_ip);
    let manager = SandboxManager::new(
        api,
        SandboxManagerConfig {
            poll_interval: Duration::from_millis(1),
            activation_timeout: Duration::from_secs(1),
            pod_ip_timeout: Duration::from_millis(3),
            ..SandboxManagerConfig::default()
        },
    );

    let error = manager
        .ensure_active("claim-no-ip", &CancellationToken::new())
        .await
        .unwrap_err();
    assert_pod_ip_deadline(&error, "claim-no-ip");
}

#[tokio::test(start_paused = true)]
async fn pod_ip_deadline_during_api_lookup_preserves_missing_ip_context() {
    let api = Arc::new(MockApi::default());
    api.state.lock().hold_get_call = Some(1);
    let manager = test_manager(api.clone());
    let started = tokio::time::Instant::now();

    let error = manager
        .wait_for_pod_ip(
            "claim-no-ip",
            missing_ip_record("claim-no-ip"),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();

    // The first poll finished and get() stayed pending until the pod-IP deadline.
    assert_eq!(api.state.lock().get_calls, 1);
    assert_eq!(started.elapsed(), manager.config.pod_ip_timeout);
    assert_pod_ip_deadline(&error, "claim-no-ip");
}

#[tokio::test(start_paused = true)]
async fn pod_ip_poll_cancellation_is_not_reported_as_a_timeout() {
    let api = Arc::new(MockApi::default());
    let cancel = CancellationToken::new();
    cancel.cancel();

    let error = test_manager(api.clone())
        .wait_for_pod_ip("cancelled", missing_ip_record("cancelled"), &cancel)
        .await
        .unwrap_err();

    assert_eq!(
        error.downcast_ref::<TerminalError>().unwrap().end_reason,
        EndReason::Cancelled
    );
    assert!(!error.to_string().contains("has no pod IP"));
    assert_eq!(api.state.lock().get_calls, 0);
}

#[tokio::test(start_paused = true)]
async fn pod_ip_api_cancellation_is_not_reported_as_a_timeout() {
    let api = Arc::new(MockApi::default());
    api.state.lock().hold_get_call = Some(1);
    let manager = test_manager(api.clone());
    let cancel = CancellationToken::new();
    let waiting = manager.wait_for_pod_ip("cancelled", missing_ip_record("cancelled"), &cancel);
    tokio::pin!(waiting);

    tokio::select! {
        biased;
        _ = api.get_started.notified() => cancel.cancel(),
        result = &mut waiting => panic!("lookup ended before cancellation: {result:?}"),
    }
    let error = waiting.await.unwrap_err();

    assert_eq!(
        error.downcast_ref::<TerminalError>().unwrap().end_reason,
        EndReason::Cancelled
    );
    assert!(!error.to_string().contains("has no pod IP"));
    assert_eq!(api.state.lock().get_calls, 1);
}

#[tokio::test(start_paused = true)]
async fn pod_ip_api_failures_are_not_reported_as_a_deadline() {
    let request_timeout = crate::kubernetes::scripted_kubernetes_error(kube::Error::Service(
        std::io::Error::new(std::io::ErrorKind::TimedOut, "scripted Kubernetes error").into(),
    ));
    for (api_error, reason, kind) in [
        (
            typed_api_error(403, 0),
            EndReason::Failed,
            FailureKind::Permanent,
        ),
        (
            request_timeout,
            EndReason::AttemptsExhausted,
            FailureKind::Timeout,
        ),
    ] {
        let api = Arc::new(MockApi::default());
        api.state.lock().scripted_get_errors.push_back(api_error);
        let manager = test_manager(api.clone()).with_backoff(BackoffConfig::new(
            Duration::from_secs(1),
            Duration::from_secs(1),
            1,
        ));

        let error = manager
            .wait_for_pod_ip(
                "claim-no-ip",
                missing_ip_record("claim-no-ip"),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let terminal = error.downcast_ref::<TerminalError>().unwrap();

        assert_eq!((terminal.end_reason, terminal.last_kind), (reason, kind));
        assert!(error.to_string().contains("scripted Kubernetes error"));
        assert!(!error.to_string().contains("has no pod IP"));
        assert_eq!(api.state.lock().get_calls, 1);
    }
}

#[tokio::test(start_paused = true)]
async fn pod_ip_missing_claim_is_not_reported_as_a_timeout() {
    let api = Arc::new(MockApi::default());
    let error = test_manager(api)
        .wait_for_pod_ip(
            "deleted",
            missing_ip_record("deleted"),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();

    assert_eq!(error.to_string(), "sandbox not found: deleted");
}

fn missing_ip_record(id: &str) -> SandboxRecord {
    let mut record = record(id, Some(1), None, Utc::now());
    record.pod_ips.clear();
    record
}

fn assert_pod_ip_deadline(error: &anyhow::Error, id: &str) {
    assert_eq!(
        error.to_string(),
        format!("sandbox {id} has no pod IP yet; it may still be starting")
    );
    let terminal = error.downcast_ref::<TerminalError>().unwrap();
    assert_eq!(terminal.end_reason, EndReason::DeadlineExceeded);
    assert_eq!(terminal.last_kind, FailureKind::Timeout);
    assert_eq!(terminal.phase, "wait_for_pod_ip");
}

#[tokio::test(start_paused = true)]
async fn already_cancelled_activation_starts_no_kubernetes_request() {
    let api = Arc::new(MockApi::default());
    api.insert(record("cancelled", Some(1), None, Utc::now()));
    let cancel = CancellationToken::new();
    cancel.cancel();

    let error = test_manager(api.clone())
        .ensure_active("cancelled", &cancel)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("Cancelled"));
    assert_eq!(api.state.lock().get_calls, 0);
}
