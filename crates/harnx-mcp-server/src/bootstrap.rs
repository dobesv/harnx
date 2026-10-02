//! Process-wide configuration and single-cluster routing for MCP connections.

use anyhow::{ensure, Context, Result};
use harnx_core::abort::AbortSignal;
use harnx_runtime::{
    config::{Config, NatsRouting, LOCAL_CLUSTER_KEY},
    local_orchestrator::{activation_route_for_cluster_with_config_dir, LocalWorkerSupervisor},
    nats_worker::{tool_reservation::ToolReservationView, SessionActivationRoute},
    tool_reservation_client::ToolReservationHandle,
};
use std::path::PathBuf;
use tokio::sync::Mutex;

/// Shared by all connections. Each reservation gets its own clone of the config.
/// The supervisor stays alive until the process-wide bootstrap is dropped.
pub struct Bootstrap {
    cluster: String,
    config: Config,
    config_dir: Option<PathBuf>,
    local_worker: Mutex<Option<LocalWorkerSupervisor>>,
    abort_signal: AbortSignal,
}

impl Bootstrap {
    /// Load configuration and validate the selected cluster at startup.
    /// Local worker startup is deferred until a connection requests a route.
    pub async fn new(
        cluster_arg: Option<String>,
        config_dir: Option<PathBuf>,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        let config_path = config_dir
            .as_ref()
            .map(|dir| dir.join("config.yaml"))
            .unwrap_or_else(Config::config_file);
        let mut config = Config::load_from_file(&config_path)
            .with_context(|| format!("failed to load config from {}", config_path.display()))?;
        config.apply_frontend_nats_routing();
        let cluster = resolve_target_cluster(&config, cluster_arg.as_deref())?;
        if cluster != LOCAL_CLUSTER_KEY {
            // Preserve the runtime's existing unknown-cluster error.
            config.nats_server(&cluster)?;
        }
        // Never reread frontend routing for a connection: CLI selection wins
        // for session storage, reservations, discovery, and calls alike.
        config.nats_routing = if cluster == LOCAL_CLUSTER_KEY {
            NatsRouting::FrontendLocal
        } else {
            NatsRouting::Cluster(cluster.clone())
        };
        tracing::info!(resolved_cluster = %cluster, "resolved target cluster");
        Ok(Self {
            cluster,
            config,
            config_dir,
            local_worker: Mutex::new(None),
            abort_signal,
        })
    }

    pub fn cluster(&self) -> &str {
        &self.cluster
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Return a valid route, ensuring the local child is healthy when needed.
    /// Concurrent callers share one supervisor through its internal mutex.
    /// No placeholder route is exposed before local worker startup succeeds.
    pub async fn ensure_route(&self) -> Result<SessionActivationRoute> {
        activation_route_for_cluster_with_config_dir(
            &self.cluster,
            &self.local_worker,
            self.abort_signal.clone(),
            self.config_dir.as_deref(),
        )
        .await
    }

    /// Open a connection-local reservation on this process's selected cluster.
    pub async fn open_reservation(
        &self,
        view: ToolReservationView,
    ) -> Result<ToolReservationHandle> {
        ToolReservationHandle::open(self.config.clone(), self.ensure_route().await?, view).await
    }
}

/// Resolve CLI override first, then frontend routing, then the local default.
pub fn resolve_target_cluster(config: &Config, cluster_arg: Option<&str>) -> Result<String> {
    match cluster_arg {
        Some(name) => {
            let name = name.trim();
            ensure!(!name.is_empty(), "--cluster value must not be empty");
            Ok(name.to_owned())
        }
        None => Ok(config.default_cluster_key().to_owned()),
    }
}
