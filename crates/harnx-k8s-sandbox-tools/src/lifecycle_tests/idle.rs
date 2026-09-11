use super::*;

#[tokio::test]
async fn idle_scan_hibernates_only_running_inactive_sandboxes() {
    let api = Arc::new(MockApi::default());
    api.insert(record(
        "idle",
        Some(1),
        None,
        Utc::now() - chrono::Duration::hours(1),
    ));
    api.insert(record("active", Some(1), None, Utc::now()));
    api.insert(record(
        "already-asleep",
        Some(0),
        None,
        Utc::now() - chrono::Duration::hours(1),
    ));

    let hibernated = test_manager(api.clone()).scan_idle().await;

    assert_eq!(api.state.lock().replica_updates, [("idle".to_string(), 0)]);
    assert_eq!(hibernated, ["idle"]);
}

#[tokio::test]
async fn idle_scan_rechecks_activity_before_hibernating() {
    let api = Arc::new(MockApi::default());
    api.insert(record(
        "became-active",
        Some(1),
        None,
        Utc::now() - chrono::Duration::hours(1),
    ));
    api.state.lock().scripted_gets.push_back(Some(record(
        "became-active",
        Some(1),
        None,
        Utc::now(),
    )));

    let hibernated = test_manager(api.clone()).scan_idle().await;

    assert!(hibernated.is_empty());
    assert!(api.state.lock().replica_updates.is_empty());
}

#[tokio::test(start_paused = true)]
async fn idle_watcher_cancellation_interrupts_midflight_scan() {
    let api = Arc::new(MockApi::default());
    api.state.lock().hold_list_call = Some(2);
    let manager = SandboxManager::new(
        api.clone(),
        SandboxManagerConfig {
            scan_interval: Duration::from_secs(10),
            ..SandboxManagerConfig::default()
        },
    );
    let cancel = CancellationToken::new();
    let watcher_cancel = cancel.clone();
    let (hibernated_tx, mut hibernated_rx) = tokio::sync::mpsc::unbounded_channel();
    let first_scan = api.list_started.notified();
    let watcher = tokio::spawn(async move {
        manager
            .run_idle_watcher(watcher_cancel, hibernated_tx)
            .await;
    });

    first_scan.await;
    let second_scan = api.list_started.notified();
    tokio::time::advance(Duration::from_secs(10)).await;
    second_scan.await;
    cancel.cancel();

    tokio::time::timeout(Duration::from_secs(1), watcher)
        .await
        .expect("watcher must stop when an in-flight scan is cancelled")
        .expect("watcher task must not panic");
    assert_eq!(api.state.lock().list_calls, 2);
    assert_eq!(hibernated_rx.recv().await, None);
}
