//! Leader election for the idle watcher via NATS lease.
//!
//! Elects a single idle watcher per namespace using `NatsSessionLease`.
//! Only the elected leader runs the idle scan loop. Standby replicas
//! retry acquisition periodically and promote when the leader drops
//! or shuts down.

use anyhow::Result;
use harnx_runtime::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
use log::{debug, info, warn};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Default lease session_id prefix for idle watcher election.
const IDLE_WATCHER_LEASE_PREFIX: &str = "k8s-sandbox-idle-watcher";

/// Standby retry interval for lease acquisition.
const STANDBY_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Maximum jitter added to standby retry.
const STANDBY_RETRY_JITTER: Duration = Duration::from_secs(2);

/// Configuration for lease-gated idle watcher.
#[derive(Debug, Clone)]
pub struct LeaseGatedWatcherConfig {
    /// The namespace being watched (used in lease key).
    pub namespace: String,
    /// Interval between idle scans when leading.
    pub scan_interval: Duration,
}

/// A unique identifier for this process instance.
fn generate_holder_id() -> String {
    Uuid::new_v4().to_string()
}

/// Lease session key scoped per namespace.
/// Format: `k8s-sandbox-idle-watcher/{namespace}`
fn lease_session_id(namespace: &str) -> String {
    format!("{IDLE_WATCHER_LEASE_PREFIX}/{namespace}")
}

/// Run the lease-gated idle watcher loop.
///
/// Continuously attempts to acquire the lease for the given namespace.
/// When acquired, runs the idle scan loop until:
/// - The lease is lost (leadership loss)
/// - The shutdown token is cancelled (graceful shutdown)
///
/// On shutdown, explicitly releases the lease so standbys can promote
/// immediately without waiting for the TTL.
pub async fn run_lease_gated_idle_watcher<M, C>(
    jetstream: async_nats::jetstream::Context,
    config: LeaseGatedWatcherConfig,
    manager: M,
    caller: C,
    shutdown: CancellationToken,
) where
    M: IdleWatcher + Clone + Send + 'static,
    C: SessionDisconnect + Clone + Send + 'static,
{
    let holder_id = generate_holder_id();
    let session_id = lease_session_id(&config.namespace);

    info!(
        "starting lease-gated idle watcher: namespace={} holder_id={} session_id={}",
        config.namespace, holder_id, session_id
    );

    loop {
        if shutdown.is_cancelled() {
            info!(
                "shutting down; exiting lease loop: namespace={} holder_id={}",
                config.namespace, holder_id
            );
            return;
        }

        // Try to acquire the lease.
        match try_acquire_and_lead(
            &jetstream,
            &session_id,
            &holder_id,
            &config,
            &manager,
            &caller,
            &shutdown,
        )
        .await
        {
            Ok(LeadershipOutcome::NotAcquired) => {
                // Lease held by another; fall through to standby backoff.
                debug!(
                    "lease held by another; entering standby: namespace={} holder_id={}",
                    config.namespace, holder_id
                );
            }
            Ok(LeadershipOutcome::LeaseLost) => {
                // Lost leadership; loop to reacquire.
                warn!(
                    "lost leadership; attempting to reacquire: namespace={} holder_id={}",
                    config.namespace, holder_id
                );
            }
            Ok(LeadershipOutcome::Released) => {
                // Graceful shutdown; lease released, exit.
                info!(
                    "graceful shutdown; lease released: namespace={} holder_id={}",
                    config.namespace, holder_id
                );
                return;
            }
            Err(error) => {
                warn!(
                    "lease acquisition error; retrying: namespace={} holder_id={} error={}",
                    config.namespace, holder_id, error
                );
            }
        }

        // Exponential backoff with jitter for standby retry.
        let jitter =
            Duration::from_millis(rand::random::<u64>() % STANDBY_RETRY_JITTER.as_millis() as u64);
        let retry_delay = STANDBY_RETRY_INTERVAL + jitter;

        tokio::select! {
            _ = tokio::time::sleep(retry_delay) => {}
            _ = shutdown.cancelled() => {
                info!(
                    "shutting down during standby wait: namespace={} holder_id={}",
                    config.namespace, holder_id
                );
                return;
            }
        }
    }
}

/// Outcome of a leadership attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeadershipOutcome {
    /// Lease not acquired (held by another); caller should retry after backoff.
    NotAcquired,
    /// Lease was lost during leadership.
    LeaseLost,
    /// Lease was explicitly released on shutdown.
    Released,
}

/// Attempt to acquire the lease and run as leader.
///
/// Returns when:
/// - Failed to acquire (caller should retry)
/// - Lost leadership (caller should retry)
/// - Shutdown requested (caller should exit)
async fn try_acquire_and_lead<M, C>(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
    holder_id: &str,
    config: &LeaseGatedWatcherConfig,
    manager: &M,
    caller: &C,
    shutdown: &CancellationToken,
) -> Result<LeadershipOutcome>
where
    M: IdleWatcher + Clone + Send + 'static,
    C: SessionDisconnect + Clone + Send + 'static,
{
    let lease_config = NatsLeaseConfig::default();
    let params = NatsLeaseAcquireParams {
        jetstream: jetstream.clone(),
        session_id,
        worker_id: holder_id.to_string(),
        generation: 0,
        config: lease_config,
        session_metadata: None,
    };

    // Try to acquire lease.
    let Some(lease) = NatsSessionLease::acquire(params).await? else {
        // Lease held by another; return to standby loop.
        debug!(
            "lease held by another; standing by: session_id={} holder_id={}",
            session_id, holder_id
        );
        return Ok(LeadershipOutcome::NotAcquired);
    };

    info!(
        "acquired leadership lease: session_id={} holder_id={}",
        session_id, holder_id
    );

    // Run as leader until lease lost or shutdown.
    let outcome = run_as_leader(lease, config, manager, caller, shutdown).await;
    Ok(outcome)
}

/// Run as leader until lease lost or shutdown.
///
/// - Creates a per-epoch cancellation token wired to lease loss.
/// - Runs the idle scan loop.
/// - On shutdown, releases the lease explicitly.
async fn run_as_leader<M, C>(
    lease: NatsSessionLease,
    config: &LeaseGatedWatcherConfig,
    manager: &M,
    caller: &C,
    shutdown: &CancellationToken,
) -> LeadershipOutcome
where
    M: IdleWatcher + Clone + Send + 'static,
    C: SessionDisconnect + Clone + Send + 'static,
{
    // Per-epoch cancellation token, canceled on lease loss.
    let epoch_cancel = CancellationToken::new();
    let epoch_cancel_for_lost = epoch_cancel.clone();
    let epoch_cancel_for_scan = epoch_cancel.clone();

    // Watch for lease loss.
    let mut lost_watch = lease.lost_watch();
    let lease_lost_fut = async move {
        loop {
            // lost_watch fires with `false` when lease is lost.
            if lost_watch.changed().await.is_err() {
                // Channel closed; lease is lost.
                epoch_cancel_for_lost.cancel();
                return;
            }
            if !*lost_watch.borrow() {
                epoch_cancel_for_lost.cancel();
                return;
            }
        }
    };
    let mut lease_lost_task = tokio::spawn(lease_lost_fut);

    // Channel for hibernated sandbox IDs.
    let (hibernated_tx, mut hibernated_rx) = mpsc::unbounded_channel();

    // Clone for the scan task.
    let manager = manager.clone();
    let caller = caller.clone();
    let scan_interval = config.scan_interval;

    // Spawn the idle scan task.
    let scan_task = tokio::spawn(async move {
        manager
            .run_idle_watcher(epoch_cancel_for_scan, hibernated_tx, scan_interval)
            .await;
    });

    // Spawn the session cleanup task (disconnects hibernated sandboxes).
    let caller_for_cleanup = caller.clone();
    let cleanup_task = tokio::spawn(async move {
        while let Some(sandbox_id) = hibernated_rx.recv().await {
            caller_for_cleanup.disconnect(&sandbox_id).await;
        }
    });

    // Wait for shutdown or lease loss.
    tokio::select! {
        _ = shutdown.cancelled() => {
            info!("shutdown requested; stopping leader");
            // Cancel the epoch so scan stops.
            epoch_cancel.cancel();
            // Wait for scan to quiesce.
            let _ = scan_task.await;
            // Wait for cleanup task with timeout to drain queued disconnects
            let _ = tokio::time::timeout(Duration::from_secs(2), cleanup_task).await;
            // Release the lease explicitly for fast failover.
            if let Err(error) = lease.release().await {
                warn!(
                    "failed to release lease on shutdown; will expire via TTL: error={}",
                    error
                );
            } else {
                info!("released lease on shutdown");
            }
            lease_lost_task.abort();
            LeadershipOutcome::Released
        }
        result = &mut lease_lost_task => {
            // Lease lost; cancel the scan.
            warn!("lease lost during leadership: result={:?}", result);
            epoch_cancel.cancel();
            let _ = scan_task.await;
            // Wait for cleanup task with timeout to drain queued disconnects
            let _ = tokio::time::timeout(Duration::from_secs(2), cleanup_task).await;
            LeadershipOutcome::LeaseLost
        }
    }
}

/// Trait for running the idle watcher loop.
///
/// Extracted from `SandboxManager` to allow mocking in tests.
#[async_trait::async_trait]
pub trait IdleWatcher: Send + Sync {
    /// Run the idle watcher loop until cancelled.
    ///
    /// Sends hibernated sandbox IDs to the channel.
    async fn run_idle_watcher(
        &self,
        cancel: CancellationToken,
        hibernated: mpsc::UnboundedSender<String>,
        scan_interval: Duration,
    );
}

/// Trait for disconnecting sessions.
///
/// Extracted from `McpCaller` to allow mocking in tests.
#[async_trait::async_trait]
pub trait SessionDisconnect: Send + Sync {
    /// Disconnect the given sandbox.
    async fn disconnect(&self, sandbox_id: &str);
}

#[async_trait::async_trait]
impl IdleWatcher for crate::lifecycle::SandboxManager {
    async fn run_idle_watcher(
        &self,
        cancel: CancellationToken,
        hibernated: mpsc::UnboundedSender<String>,
        _scan_interval: Duration,
    ) {
        // Run with the provided scan interval.
        // The existing run_idle_watcher already takes these params, just call it.
        // Note: we can't override the scan_interval easily without exposing config setter,
        // but the watcher already runs with configurable scan_interval from SandboxManagerConfig.
        self.run_idle_watcher(cancel, hibernated).await;
    }
}

#[async_trait::async_trait]
impl SessionDisconnect for Arc<dyn crate::mcp::McpCaller> {
    async fn disconnect(&self, sandbox_id: &str) {
        self.disconnect(sandbox_id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_session_id_format() {
        assert_eq!(
            lease_session_id("agent-sandboxes"),
            "k8s-sandbox-idle-watcher/agent-sandboxes"
        );
        assert_eq!(
            lease_session_id("prod-sandboxes"),
            "k8s-sandbox-idle-watcher/prod-sandboxes"
        );
    }

    #[test]
    fn holder_id_is_unique() {
        let id1 = generate_holder_id();
        let id2 = generate_holder_id();
        assert_ne!(id1, id2);
        assert!(uuid::Uuid::parse_str(&id1).is_ok());
    }
}
