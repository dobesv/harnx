//! Worker connection, activation, and child-server configuration.

use super::activation::SessionActivationRoute;
use super::activation_transport::{validate_worker_id, WORK_NOTIFY_ACK_WAIT};
use crate::config::LOCAL_CLUSTER_KEY;
use crate::nats_lease::NatsLeaseConfig;
use anyhow::Result;
use harnx_core::instance::{ServerScope, HARNX_SERVER_SCOPE};
use std::time::Duration;

/// How a worker resolves its NATS connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NatsConnectionSource {
    /// A persistent cluster from `nats_servers/<name>.yaml`.
    ConfiguredCluster(String),
    /// The frontend-provided `HARNX_NATS_URL` / `HARNX_NATS_TOKEN` handoff.
    LocalEnvironment,
}

impl NatsConnectionSource {
    pub fn key(&self) -> &str {
        match self {
            Self::ConfiguredCluster(cluster) => cluster,
            Self::LocalEnvironment => LOCAL_CLUSTER_KEY,
        }
    }
}

/// Which activation transport a worker consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerActivationMode {
    ClusterShared,
    WorkerTargeted,
}

/// How long a busy activation may stay parked on a worker before it is handed
/// back to JetStream. Long enough to cover a legitimate turn well past staging
/// p90 (73 minutes), short enough that a session held by a stuck worker gives
/// its queued activations back to the broker within a working day.
pub(super) const DEFAULT_BUSY_WAIT_MAX: Duration = Duration::from_secs(6 * 60 * 60);
/// Parked busy waiters per session by default; further busy deliveries for
/// the same session fall back to a delayed NAK.
pub(super) const DEFAULT_BUSY_WAITER_CAPACITY: usize = 4;

/// Fault injection seams for broker-backed activation tests. Nothing in
/// production sets these; the claim chain only reads them at the marked
/// points so tests can exercise error paths deterministically.
#[derive(Debug, Default)]
pub struct ActivationFaults {
    /// Makes the targeted lease-holder preflight read fail once set.
    pub fail_targeted_lease_holder_read: std::sync::atomic::AtomicBool,
    /// Makes the targeted activation lease acquisition fail once set.
    pub fail_targeted_lease_acquire: std::sync::atomic::AtomicBool,
    /// Makes every claim attempt fail while set.
    pub fail_claims: std::sync::atomic::AtomicBool,
    /// Keeps busy waiters from creating their lease-key watch, so recovery
    /// runs on the recheck timer alone.
    pub drop_lease_watch: std::sync::atomic::AtomicBool,
    /// Returns busy claims with the delivery's heartbeat already stopped,
    /// simulating a wind-up lease lost mid-claim, so tests exercise the
    /// dead-heartbeat park rejections.
    pub busy_claims_dead_heartbeat: std::sync::atomic::AtomicBool,
    /// Holds a waiter's shutdown disposition flush open forever, so the
    /// shutdown settle timeout must abort and join the waiter.
    pub stall_shutdown_flush: std::sync::atomic::AtomicBool,
}

/// Live view of the busy-waiter registry for broker-backed tests: how many
/// sessions have slots and how many deliveries are parked. Updated whenever
/// the registry changes, so tests can assert cleanup, not just the gauge.
#[derive(Debug, Default)]
pub struct BusyWaiterProbe {
    pub slots: std::sync::atomic::AtomicUsize,
    pub parked: std::sync::atomic::AtomicUsize,
}

/// Configuration for a worker daemon instance.
#[derive(Debug, Clone)]
pub struct WorkerDaemonConfig {
    pub connection: NatsConnectionSource,
    pub session_scope: String,
    pub activation_mode: WorkerActivationMode,
    pub worker_id: String,
    pub lease: NatsLeaseConfig,
    /// Whether this worker launches its own tool and hook servers as child
    /// processes instead of discovering independently deployed ones.
    pub manage_servers: bool,
    activation_ack_wait: Duration,
    /// How long a parked busy waiter may hold an activation before handing it
    /// back to JetStream for a delayed redelivery.
    pub(super) busy_wait_max: Duration,
    /// Parked busy waiters allowed per session. Further busy deliveries fall
    /// back to a delayed NAK and stay owned by JetStream instead.
    pub(super) busy_waiter_capacity: usize,
    /// Frontends renew every third of this window. Lost replies and crashes
    /// cannot keep a reservation alive indefinitely.
    pub(super) tool_reservation_ttl: Duration,
    /// Keep idle processes briefly so back-to-back sessions reuse them instead
    /// of paying startup and registration costs again.
    pub(super) tool_server_linger: Duration,
    /// Test-only activation fault seams (see `ActivationFaults`).
    pub(super) activation_faults: Option<std::sync::Arc<ActivationFaults>>,
    /// Test-only busy-waiter registry view (see `BusyWaiterProbe`).
    pub(super) busy_waiter_probe: Option<std::sync::Arc<BusyWaiterProbe>>,
}

impl WorkerDaemonConfig {
    /// Shared construction for the public entry points: everything except the
    /// connection/activation-routing choices is the same for every worker.
    fn base(
        connection: NatsConnectionSource,
        session_scope: String,
        activation_mode: WorkerActivationMode,
        worker_id: String,
    ) -> Self {
        Self {
            connection,
            session_scope,
            activation_mode,
            worker_id,
            lease: NatsLeaseConfig::default(),
            manage_servers: false,
            activation_ack_wait: WORK_NOTIFY_ACK_WAIT,
            busy_wait_max: DEFAULT_BUSY_WAIT_MAX,
            busy_waiter_capacity: DEFAULT_BUSY_WAITER_CAPACITY,
            tool_reservation_ttl: Duration::from_secs(60),
            tool_server_linger: Duration::from_secs(60),
            activation_faults: None,
            busy_waiter_probe: None,
        }
    }

    pub fn new(cluster: impl Into<String>, worker_id: impl Into<String>) -> Self {
        let cluster = cluster.into();
        Self::base(
            NatsConnectionSource::ConfiguredCluster(cluster.clone()),
            cluster,
            WorkerActivationMode::ClusterShared,
            worker_id.into(),
        )
    }

    pub fn managing(cluster: impl Into<String>, worker_id: impl Into<String>) -> Self {
        Self {
            manage_servers: true,
            ..Self::new(cluster, worker_id)
        }
    }

    pub fn local(worker_id: impl Into<String>) -> Result<Self> {
        let worker_id = worker_id.into();
        validate_worker_id(&worker_id)?;
        Ok(Self {
            manage_servers: true,
            ..Self::base(
                NatsConnectionSource::LocalEnvironment,
                LOCAL_CLUSTER_KEY.to_string(),
                WorkerActivationMode::WorkerTargeted,
                worker_id,
            )
        })
    }

    pub fn connection_key(&self) -> &str {
        self.connection.key()
    }

    pub fn activation_route(&self) -> SessionActivationRoute {
        match self.activation_mode {
            WorkerActivationMode::ClusterShared => SessionActivationRoute::ClusterShared,
            WorkerActivationMode::WorkerTargeted => SessionActivationRoute::WorkerTargeted {
                session_scope: self.session_scope.clone(),
                worker_id: self.worker_id.clone(),
            },
        }
    }

    /// Shortens reservation expiry and server teardown in broker-backed tests.
    #[doc(hidden)]
    pub fn with_tool_reservation_timing_for_test(
        mut self,
        ttl: Duration,
        linger: Duration,
    ) -> Self {
        assert!(ttl >= Duration::from_millis(3));
        self.tool_reservation_ttl = ttl;
        self.tool_server_linger = linger;
        self
    }

    pub(super) fn activation_ack_wait(&self) -> Duration {
        self.activation_ack_wait
    }

    /// Overrides JetStream activation timing in broker-backed tests.
    #[doc(hidden)]
    pub fn with_activation_ack_wait_for_test(mut self, ack_wait: Duration) -> Self {
        assert!(ack_wait >= Duration::from_millis(3));
        self.activation_ack_wait = ack_wait;
        self
    }

    pub(super) fn busy_wait_max(&self) -> Duration {
        self.busy_wait_max
    }

    pub(super) fn busy_waiter_capacity(&self) -> usize {
        self.busy_waiter_capacity
    }

    /// Overrides busy-waiter limits in broker-backed tests.
    #[doc(hidden)]
    pub fn with_busy_wait_limits_for_test(
        mut self,
        max_wait: Duration,
        capacity_per_session: usize,
    ) -> Self {
        assert!(max_wait >= Duration::from_millis(1));
        self.busy_wait_max = max_wait;
        self.busy_waiter_capacity = capacity_per_session;
        self
    }

    pub(super) fn activation_faults(&self) -> Option<std::sync::Arc<ActivationFaults>> {
        self.activation_faults.clone()
    }

    pub(super) fn busy_waiter_probe(&self) -> Option<std::sync::Arc<BusyWaiterProbe>> {
        self.busy_waiter_probe.clone()
    }

    /// Wires test-only fault seams into the claim chain.
    #[doc(hidden)]
    pub fn with_activation_faults_for_test(
        mut self,
        faults: std::sync::Arc<ActivationFaults>,
    ) -> Self {
        self.activation_faults = Some(faults);
        self
    }

    /// Wires a test-only live view of the busy-waiter registry.
    #[doc(hidden)]
    pub fn with_busy_waiter_probe_for_test(
        mut self,
        probe: std::sync::Arc<BusyWaiterProbe>,
    ) -> Self {
        self.busy_waiter_probe = Some(probe);
        self
    }
}

/// Resolve the scope this worker addresses servers under.
pub fn resolve_worker_scope(manage_servers: bool) -> Result<ServerScope> {
    if manage_servers {
        return Ok(ServerScope::new());
    }
    std::env::var(HARNX_SERVER_SCOPE)
        .map(ServerScope::from_string)
        .map_err(|_| {
            anyhow::anyhow!(
                "{HARNX_SERVER_SCOPE} is required without --manage-servers: a worker \
                 that does not launch its own servers must be told which scope to \
                 discover them under"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_daemon_has_explicit_connection_scope_and_activation_mode() {
        let daemon = WorkerDaemonConfig::local("local-test").expect("local daemon config");
        assert_eq!(daemon.connection, NatsConnectionSource::LocalEnvironment);
        assert_eq!(daemon.connection_key(), LOCAL_CLUSTER_KEY);
        assert_eq!(daemon.session_scope, LOCAL_CLUSTER_KEY);
        assert_eq!(daemon.activation_mode, WorkerActivationMode::WorkerTargeted);
        assert!(daemon.manage_servers);
        assert_eq!(
            daemon.activation_route(),
            SessionActivationRoute::WorkerTargeted {
                session_scope: LOCAL_CLUSTER_KEY.to_string(),
                worker_id: "local-test".to_string(),
            }
        );
    }
}
