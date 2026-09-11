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
    assert_eq!(state.activity_updates, ["claim-2"]);
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

#[tokio::test]
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
    assert!(error.to_string().contains("has no pod IP"));
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
