use super::*;

// ============================================================================
// Regression tests for GitHub #2405: idle watcher suspends sandbox mid-wake
// ============================================================================

/// Test A: idle scan during wake does NOT suspend a waking sandbox.
///
/// A sandbox with stale activity is hibernated. When ensure_active wakes it,
/// an idle scan running during the wake should NOT suspend it because the
/// pre-wake activity bump will protect it.
///
/// Expected: no set_replicas(0) from the idle scan.
#[tokio::test]
async fn idle_scan_during_wake_does_not_suspend_waking_sandbox() {
    let api = Arc::new(MockApi::default());
    let old_activity = Utc::now() - chrono::Duration::hours(2);

    // Create a hibernated record WITHOUT Ready condition, so ensure_active
    // stays pending until we insert a ready record.
    let mut hibernated = record("test-claim", Some(0), None, old_activity);
    hibernated.sandbox_name = Some("sandbox-test-claim".to_string());
    hibernated.created_at = Some(Utc::now() - chrono::Duration::hours(1));
    hibernated.conditions.clear(); // No Ready condition => pending
    api.insert(hibernated);

    let manager = SandboxManager::new(
        api.clone(),
        SandboxManagerConfig {
            poll_interval: Duration::from_millis(1),
            activation_timeout: Duration::from_secs(5),
            pod_ip_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(60),
            ..SandboxManagerConfig::default()
        },
    );

    // Spawn ensure_active in background
    let ensure_active_task = tokio::spawn(async move {
        manager
            .ensure_active("test-claim", &CancellationToken::new())
            .await
    });

    // Wait for the wake (set_replicas(1)) to complete
    tokio::time::timeout(Duration::from_secs(2), async {
        api.set_replicas_notify.notified().await
    })
    .await
    .expect("wake should complete within timeout");

    // Clear replica_updates to detect any new suspension
    api.state.lock().replica_updates.clear();

    // Run idle scan - should NOT suspend the waking sandbox
    let _ = test_manager(api.clone()).scan_idle().await;

    // No suspension because pre-wake bump updated last_activity
    assert!(
        !api.state
            .lock()
            .replica_updates
            .iter()
            .any(|(id, r)| id == "test-claim" && *r == 0),
        "idle scan should NOT suspend a sandbox that is currently being woken, but got {:?}",
        api.state.lock().replica_updates.clone()
    );

    // Make the record ready so ensure_active completes (includes pod IP)
    let ready_record = record("test-claim", Some(1), None, Utc::now());
    api.state
        .lock()
        .records
        .insert("test-claim".to_string(), ready_record);

    // ensure_active should succeed
    let result = tokio::time::timeout(Duration::from_secs(2), ensure_active_task)
        .await
        .expect("ensure_active should complete")
        .expect("ensure_active task should not panic");
    assert!(result.is_ok(), "ensure_active should succeed: {:?}", result);
}

/// Test B: ensure_active re-wakes sandbox that gets hibernated during wait.
///
/// After the first wake, the record flips back to replicas=0 (simulating
/// an idle suspension). The sandbox should be re-woken and then succeed.
///
/// Expected: exactly two set_replicas(1) calls.
#[tokio::test]
async fn ensure_active_rewakes_sandbox_hibernated_during_wait() {
    let api = Arc::new(MockApi::default());

    let old_activity = Utc::now() - chrono::Duration::hours(1);
    let mut hibernated = record("test-claim", Some(0), None, old_activity);
    hibernated.sandbox_name = Some("sandbox-test-claim".to_string());
    hibernated.created_at = Some(Utc::now() - chrono::Duration::hours(1));
    api.insert(hibernated.clone());

    // Script sequence: MockApi::get() pops scripted_gets BEFORE state.records.
    // Initial get() needs to see hibernated (replicas=0) to trigger wake.
    // Then wait_until_ready polls: pending -> hibernated_again -> ready
    let pending = pending_record("test-claim");
    let mut hibernated_again = record(
        "test-claim",
        Some(0),
        None,
        Utc::now() - chrono::Duration::hours(1),
    );
    hibernated_again.sandbox_name = Some("sandbox-test-claim".to_string());
    let ready = record("test-claim", Some(1), None, Utc::now());

    api.state.lock().scripted_gets.extend([
        Some(hibernated.clone()), // Initial get sees hibernated -> triggers wake
        Some(pending),            // After wake, not ready yet
        Some(hibernated_again),   // Simulated idle suspension
        Some(ready),              // After re-wake, ready
    ]);

    let manager = SandboxManager::new(
        api.clone(),
        SandboxManagerConfig {
            poll_interval: Duration::from_millis(1),
            activation_timeout: Duration::from_secs(5),
            pod_ip_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(60),
            ..SandboxManagerConfig::default()
        },
    );

    let result = manager
        .ensure_active("test-claim", &CancellationToken::new())
        .await;

    assert!(
        result.is_ok(),
        "ensure_active should succeed after re-wake: {:?}",
        result
    );

    let replica_updates = api.state.lock().replica_updates.clone();
    let wake_count = replica_updates
        .iter()
        .filter(|(id, r)| id == "test-claim" && *r == 1)
        .count();
    assert_eq!(
        wake_count, 2,
        "expected exactly 2 wake calls (initial + re-wake), got {:?}",
        replica_updates
    );
}

/// Test C: ensure_active fails fast when sandbox keeps re-hibernating.
///
/// The record keeps returning replicas=0 after every wake. After exhausting
/// the re-wake budget (3 total wakes), ensure_active should fail fast with
/// a suspension-specific error, not wait for activation_timeout.
///
/// Expected: exactly 3 set_replicas(1) calls, then error.
#[tokio::test]
async fn ensure_active_fails_fast_when_sandbox_keeps_rehibernating() {
    let api = Arc::new(MockApi::default());

    let old_activity = Utc::now() - chrono::Duration::hours(1);
    let mut hibernated = record("test-claim", Some(0), None, old_activity);
    hibernated.sandbox_name = Some("sandbox-test-claim".to_string());
    hibernated.created_at = Some(Utc::now() - chrono::Duration::hours(1));
    api.insert(hibernated.clone());

    let make_hibernated = || {
        let mut rec = record(
            "test-claim",
            Some(0),
            None,
            Utc::now() - chrono::Duration::hours(1),
        );
        rec.sandbox_name = Some("sandbox-test-claim".to_string());
        rec.created_at = Some(Utc::now() - chrono::Duration::hours(1));
        rec
    };

    // Initial get needs to see hibernated (from scripted_gets, not state.records)
    // Then every subsequent poll returns hibernated again
    api.state
        .lock()
        .scripted_gets
        .push_back(Some(hibernated.clone())); // Initial get
    for _ in 0..10 {
        api.state
            .lock()
            .scripted_gets
            .push_back(Some(make_hibernated()));
    }

    let manager = SandboxManager::new(
        api.clone(),
        SandboxManagerConfig {
            poll_interval: Duration::from_millis(1),
            activation_timeout: Duration::from_secs(10),
            pod_ip_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(60),
            ..SandboxManagerConfig::default()
        },
    );

    let start = tokio::time::Instant::now();
    let result = manager
        .ensure_active("test-claim", &CancellationToken::new())
        .await;
    let elapsed = start.elapsed();

    let err = result.expect_err("ensure_active should fail when sandbox keeps re-hibernating");

    // Should fail fast, well before activation_timeout
    assert!(
        elapsed < Duration::from_secs(5),
        "should fail fast (<5s), took {:?}",
        elapsed
    );

    let err_str = err.to_string();
    assert_eq!(
        err_str,
        "sandbox test-claim was suspended again while waking after 2 recovery wake attempts"
    );

    let replica_updates = api.state.lock().replica_updates.clone();
    let wake_count = replica_updates
        .iter()
        .filter(|(id, r)| id == "test-claim" && *r == 1)
        .count();
    assert_eq!(
        wake_count, 3,
        "expected exactly 3 wake attempts, got {:?}",
        replica_updates
    );
}

/// Test D: wait_until_ready sends keepalive bumps at ~60s intervals.
///
/// Using paused Tokio time, the sandbox stays pending for >2 minutes,
/// then becomes ready. Activity bumps should occur roughly every 60s,
/// not on every poll.
///
/// Expected: pre-wake + 2 keep-alives (at 60s and 120s) + post-ready = 4 bumps.
#[tokio::test(start_paused = true)]
async fn wait_until_ready_sends_keepalive_bumps() {
    let api = Arc::new(MockApi::default());

    let old_activity = Utc::now() - chrono::Duration::hours(1);
    let mut hibernated = record("test-claim", Some(0), None, old_activity);
    hibernated.sandbox_name = Some("sandbox-test-claim".to_string());
    hibernated.created_at = Some(Utc::now() - chrono::Duration::hours(1));
    api.insert(hibernated.clone());

    let pending = pending_record("test-claim");
    let ready = record("test-claim", Some(1), None, Utc::now());

    // Initial get needs to see hibernated (from scripted_gets) to trigger wake.
    // Then ~150s of pending records, then ready.
    api.state
        .lock()
        .scripted_gets
        .push_back(Some(hibernated.clone())); // Initial get
    for _ in 0..31 {
        api.state
            .lock()
            .scripted_gets
            .push_back(Some(pending.clone()));
    }
    api.state.lock().scripted_gets.push_back(Some(ready));

    let manager = SandboxManager::new(
        api.clone(),
        SandboxManagerConfig {
            poll_interval: Duration::from_secs(5), // 5s polls
            activation_timeout: Duration::from_secs(300),
            pod_ip_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(60),
            ..SandboxManagerConfig::default()
        },
    );

    let result = manager
        .ensure_active("test-claim", &CancellationToken::new())
        .await;

    assert!(result.is_ok(), "ensure_active should succeed: {:?}", result);

    let activity_updates = api.state.lock().activity_updates.clone();
    let bump_count = activity_updates
        .iter()
        .filter(|id| *id == "test-claim")
        .count();

    // With ~160s of waiting and 60s keepalive interval:
    // - Pre-wake bump (before first wake)
    // - ~2 keepalive bumps (at ~60s and ~120s during the 160s wait)
    // - Post-ready bump
    // Total: exactly 4 bumps under deterministic paused time
    assert_eq!(
        bump_count, 4,
        "expected exactly 4 activity bumps (pre-wake + 2 keepalives + post-ready), got {}",
        bump_count
    );

    // Sanity check: bumps should be far fewer than poll count
    let poll_count = api.state.lock().get_calls;
    assert!(
        bump_count < poll_count,
        "bumps ({}) should be fewer than polls ({})",
        bump_count,
        poll_count
    );
}

// ============================================================================
// Existing tests
// ============================================================================

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
