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
    /// Frontends renew every third of this window. Lost replies and crashes
    /// cannot keep a reservation alive indefinitely.
    pub(super) tool_reservation_ttl: Duration,
    /// Keep idle processes briefly so back-to-back sessions reuse them instead
    /// of paying startup and registration costs again.
    pub(super) tool_server_linger: Duration,
}

impl WorkerDaemonConfig {
    pub fn new(cluster: impl Into<String>, worker_id: impl Into<String>) -> Self {
        let cluster = cluster.into();
        Self {
            connection: NatsConnectionSource::ConfiguredCluster(cluster.clone()),
            session_scope: cluster,
            activation_mode: WorkerActivationMode::ClusterShared,
            worker_id: worker_id.into(),
            lease: NatsLeaseConfig::default(),
            manage_servers: false,
            activation_ack_wait: WORK_NOTIFY_ACK_WAIT,
            tool_reservation_ttl: Duration::from_secs(60),
            tool_server_linger: Duration::from_secs(60),
        }
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
            connection: NatsConnectionSource::LocalEnvironment,
            session_scope: LOCAL_CLUSTER_KEY.to_string(),
            activation_mode: WorkerActivationMode::WorkerTargeted,
            worker_id,
            lease: NatsLeaseConfig::default(),
            manage_servers: true,
            activation_ack_wait: WORK_NOTIFY_ACK_WAIT,
            tool_reservation_ttl: Duration::from_secs(60),
            tool_server_linger: Duration::from_secs(60),
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
