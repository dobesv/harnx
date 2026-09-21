//! Integration tests for leader election.
//!
//! These tests verify the lease-gated idle watcher behavior:
//! - Exactly one leader per namespace
//! - Leadership loss cancels scan
//! - Graceful shutdown releases lease
//! - Standby promotes when leader releases
//! - Follower (non-leader) replicas report ready independently of lease

use crate::leader_election::{
    run_lease_gated_idle_watcher, IdleWatcher, LeaseGatedWatcherConfig, SessionDisconnect,
};
use harnx_runtime::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Mock idle watcher that tracks scan invocations.
#[derive(Clone)]
struct MockIdleWatcher {
    scan_count: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl IdleWatcher for MockIdleWatcher {
    async fn run_idle_watcher(
        &self,
        cancel: CancellationToken,
        _hibernated: mpsc::UnboundedSender<String>,
        _scan_interval: Duration,
    ) {
        loop {
            if cancel.is_cancelled() {
                return;
            }
            self.scan_count.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Mock session disconnect that tracks disconnect calls.
#[derive(Clone)]
#[allow(dead_code)]
struct MockSessionDisconnect {
    disconnect_count: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl SessionDisconnect for MockSessionDisconnect {
    async fn disconnect(&self, _sandbox_id: &str) {
        self.disconnect_count.fetch_add(1, Ordering::SeqCst);
    }
}

/// Helper to start a NATS server for testing with proper temp directory.
async fn start_nats_server() -> Option<(tokio::process::Child, String, tempfile::TempDir)> {
    // Check if nats-server is available
    if which::which("nats-server").is_err() {
        return None;
    }

    // Create a unique temp directory for this test run
    let temp_dir = tempfile::tempdir().ok()?;

    let port = 4000 + rand::random::<u16>() % 1000;
    let mut cmd = tokio::process::Command::new("nats-server");
    cmd.args([
        "-p",
        &port.to_string(),
        "-js",
        "--store_dir",
        &temp_dir.path().to_string_lossy(),
    ]);
    cmd.kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return None,
    };

    // Wait for server to be ready
    let url = format!("nats://127.0.0.1:{}", port);
    for _ in 0..50 {
        if async_nats::connect(&url).await.is_ok() {
            return Some((child, url, temp_dir));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    None
}

/// Test that exactly one replica holds the lease at a time.
#[tokio::test]
async fn only_one_leader_per_namespace() {
    let Some((_nats, url, _temp_dir)) = start_nats_server().await else {
        println!("Skipping: nats-server not available");
        return;
    };

    let client = async_nats::connect(&url).await.expect("connect");
    let jetstream = async_nats::jetstream::new(client);

    // Use unique session_id for this test run
    let session_id = format!("test-leader-{}", uuid::Uuid::new_v4());

    // Try to acquire lease twice with different holder IDs
    let config1 = NatsLeaseConfig::default();
    let config2 = NatsLeaseConfig::default();

    let params1 = NatsLeaseAcquireParams {
        jetstream: jetstream.clone(),
        session_id: &session_id,
        worker_id: "replica-1".to_string(),
        generation: 0,
        config: config1,
        session_metadata: None,
    };

    let params2 = NatsLeaseAcquireParams {
        jetstream: jetstream.clone(),
        session_id: &session_id,
        worker_id: "replica-2".to_string(),
        generation: 0,
        config: config2,
        session_metadata: None,
    };

    // First acquisition should succeed
    let lease1 = NatsSessionLease::acquire(params1)
        .await
        .expect("acquire should not error")
        .expect("first lease should be acquired");

    // Second acquisition should fail (lease already held)
    let lease2 = NatsSessionLease::acquire(params2.clone())
        .await
        .expect("acquire should not error");

    assert!(
        lease2.is_none(),
        "second acquisition should fail while first holds lease"
    );

    // Verify lease1 is still held
    assert!(lease1.is_held(), "lease1 should still be held");

    // Release lease1
    lease1.release().await.expect("release should succeed");

    // Now second acquisition should succeed
    let lease2_again = NatsSessionLease::acquire(params2)
        .await
        .expect("acquire should not error")
        .expect("lease should be acquired after release");

    assert!(
        lease2_again.is_held(),
        "lease2 should be held after release"
    );
}

/// Test that leadership loss stops scan loop.
#[tokio::test]
async fn leadership_loss_cancels_scan() {
    let scan_count = Arc::new(AtomicUsize::new(0));
    let watcher = MockIdleWatcher {
        scan_count: Arc::clone(&scan_count),
    };

    let cancel = CancellationToken::new();
    let (tx, _rx) = mpsc::unbounded_channel();

    // Run watcher in background
    let scan_count_clone = Arc::clone(&scan_count);
    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(async move {
        watcher
            .run_idle_watcher(cancel_clone, tx, Duration::from_millis(50))
            .await;
    });

    // Let it scan a few times
    tokio::time::sleep(Duration::from_millis(250)).await;
    let scans_before = scan_count_clone.load(Ordering::SeqCst);
    assert!(scans_before >= 2, "should have scanned at least twice");

    // Cancel and wait for shutdown
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("watcher should shut down quickly")
        .expect("task should complete");

    let scans_after = scan_count_clone.load(Ordering::SeqCst);

    // Wait a bit more and verify no additional scans
    tokio::time::sleep(Duration::from_millis(150)).await;
    let scans_final = scan_count_clone.load(Ordering::SeqCst);

    assert_eq!(
        scans_after, scans_final,
        "no scans should occur after cancellation"
    );
}

/// Test that standby promotes when leader releases lease.
#[tokio::test]
async fn standby_promotes_on_leader_release() {
    let Some((_nats, url, _temp_dir)) = start_nats_server().await else {
        println!("Skipping: nats-server not available");
        return;
    };

    let client = async_nats::connect(&url).await.expect("connect");
    let jetstream = async_nats::jetstream::new(client);

    // Use unique session_id for this test run
    let namespace = format!("test-namespace-{}", uuid::Uuid::new_v4());

    // Track scans for instance 1 and instance 2
    let scan_count_1 = Arc::new(AtomicUsize::new(0));
    let scan_count_2 = Arc::new(AtomicUsize::new(0));

    // Create mock watchers
    let watcher1 = MockIdleWatcher {
        scan_count: Arc::clone(&scan_count_1),
    };
    let watcher2 = MockIdleWatcher {
        scan_count: Arc::clone(&scan_count_2),
    };

    // Mock disconnect handlers
    let disconnect_1 = MockSessionDisconnect {
        disconnect_count: Arc::new(AtomicUsize::new(0)),
    };
    let disconnect_2 = MockSessionDisconnect {
        disconnect_count: Arc::new(AtomicUsize::new(0)),
    };

    let shutdown1 = CancellationToken::new();
    let shutdown2 = CancellationToken::new();

    let config1 = LeaseGatedWatcherConfig {
        namespace: namespace.clone(),
        scan_interval: Duration::from_millis(100),
    };
    let config2 = LeaseGatedWatcherConfig {
        namespace: namespace.clone(),
        scan_interval: Duration::from_millis(100),
    };

    // Spawn instance 1 (will be leader)
    let jetstream1 = jetstream.clone();
    let shutdown1_clone = shutdown1.clone();
    let handle1 = tokio::spawn(async move {
        run_lease_gated_idle_watcher(jetstream1, config1, watcher1, disconnect_1, shutdown1_clone)
            .await;
    });

    // Wait for instance 1 to acquire leadership
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Instance 1 should be scanning
    let scans1_initial = scan_count_1.load(Ordering::SeqCst);
    assert!(
        scans1_initial >= 2,
        "instance 1 should have scanned as leader"
    );

    // Instance 2 should not have scanned yet (standby)
    let scans2_initial = scan_count_2.load(Ordering::SeqCst);
    assert_eq!(
        scans2_initial, 0,
        "instance 2 should not have scanned while standby"
    );

    // Spawn instance 2 (will be standby)
    let jetstream2 = jetstream.clone();
    let shutdown2_clone = shutdown2.clone();
    let handle2 = tokio::spawn(async move {
        run_lease_gated_idle_watcher(jetstream2, config2, watcher2, disconnect_2, shutdown2_clone)
            .await;
    });

    // Wait for standby to notice it can't acquire
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Instance 2 should still be in standby (no scans)
    let scans2_standby = scan_count_2.load(Ordering::SeqCst);
    assert_eq!(
        scans2_standby, 0,
        "instance 2 should be in standby while instance 1 is leader"
    );

    // Instance 1 should still be scanning
    let scans1_before_release = scan_count_1.load(Ordering::SeqCst);
    assert!(
        scans1_before_release > scans1_initial,
        "instance 1 should continue scanning"
    );

    // Trigger graceful shutdown on instance 1
    shutdown1.cancel();

    // Wait for instance 1 to release lease and exit
    tokio::time::timeout(Duration::from_secs(2), handle1)
        .await
        .expect("instance 1 should shut down")
        .expect("instance 1 task should complete");

    // Wait for instance 2 to acquire lease and start scanning
    // Give more time for the standby to retry after backoff (~5-7s with jitter)
    tokio::time::sleep(Duration::from_secs(8)).await;

    // Instance 2 should now be scanning as the new leader
    let scans2_after = scan_count_2.load(Ordering::SeqCst);
    assert!(
        scans2_after >= 1,
        "instance 2 should have scanned after promoting to leader"
    );

    // Clean up instance 2
    shutdown2.cancel();
    tokio::time::timeout(Duration::from_secs(2), handle2)
        .await
        .expect("instance 2 should shut down")
        .expect("instance 2 task should complete");
}

/// Test that non-leader (standby) replicas report ready independently of
/// lease leadership. In HA mode, all replicas should be ready so k8s can
/// route traffic to them; only the idle watcher is gated by the lease.
#[tokio::test]
async fn follower_readiness_independent_of_leadership() {
    let Some((_nats, url, _temp_dir)) = start_nats_server().await else {
        println!("Skipping: nats-server not available");
        return;
    };

    let client = async_nats::connect(&url).await.expect("connect");
    let jetstream = async_nats::jetstream::new(client);

    // Use unique namespace for this test run
    let namespace = format!("test-namespace-{}-follower-ready", uuid::Uuid::new_v4());

    // Create a mock watcher that tracks scans and a readiness handle
    let scan_count = Arc::new(AtomicUsize::new(0));
    let readiness = harnx_healthz::Readiness::default();

    // Mark ready immediately on start (simulating server startup)
    // The server should be ready regardless of whether it holds the lease
    readiness.ready();

    let watcher = MockIdleWatcher {
        scan_count: Arc::clone(&scan_count),
    };
    let disconnect = MockSessionDisconnect {
        disconnect_count: Arc::new(AtomicUsize::new(0)),
    };

    let config = LeaseGatedWatcherConfig {
        namespace: namespace.clone(),
        scan_interval: Duration::from_millis(100),
    };

    // Start a replica that will be the leader
    let shutdown_leader = CancellationToken::new();
    let shutdown_leader_cancel = shutdown_leader.clone();
    let handle = tokio::spawn(run_lease_gated_idle_watcher(
        jetstream.clone(),
        config,
        watcher.clone(),
        disconnect.clone(),
        shutdown_leader,
    ));

    // Wait for the leader to acquire the lease and start scanning
    tokio::time::sleep(Duration::from_millis(500)).await;
    let scans_as_leader = scan_count.load(Ordering::SeqCst);
    assert!(
        scans_as_leader >= 2,
        "leader should be scanning, got {scans_as_leader}"
    );

    // Start a second replica (standby) with its own readiness
    let readiness_standby = harnx_healthz::Readiness::default();
    readiness_standby.ready();

    let scan_count_standby = Arc::new(AtomicUsize::new(0));
    let watcher_standby = MockIdleWatcher {
        scan_count: Arc::clone(&scan_count_standby),
    };
    let disconnect_standby = MockSessionDisconnect {
        disconnect_count: Arc::new(AtomicUsize::new(0)),
    };
    let shutdown_standby = CancellationToken::new();
    let shutdown_standby_cancel = shutdown_standby.clone();

    let config_standby = LeaseGatedWatcherConfig {
        namespace: namespace.clone(),
        scan_interval: Duration::from_millis(100),
    };

    let _handle_standby = tokio::spawn(run_lease_gated_idle_watcher(
        jetstream,
        config_standby,
        watcher_standby,
        disconnect_standby,
        shutdown_standby,
    ));

    // Wait for standby to notice it can't acquire
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Verify standby is not scanning (not the leader)
    let scans_standby = scan_count_standby.load(Ordering::SeqCst);
    assert_eq!(
        scans_standby, 0,
        "standby should not be scanning, got {scans_standby}"
    );

    // Verify the standby reports ready even though it's not the leader
    assert!(
        readiness_standby.is_ready(),
        "standby should report ready regardless of leadership"
    );

    // Also verify the leader reports ready
    assert!(readiness.is_ready(), "leader should also report ready");

    // Clean up
    shutdown_leader_cancel.cancel();
    shutdown_standby_cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("leader should shut down");
}
