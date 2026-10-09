//! Cluster services and admission gates shared by exported agents.
use crate::{runner::Runner, store::A2aStore};
use harnx_core::abort::AbortSignal;
use harnx_runtime::local_orchestrator::{
    activation_route_for_cluster_with_config_dir, LocalWorkerSupervisor,
};
use harnx_runtime::{config::GlobalConfig, SessionActivationRoute};
use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};
use tokio::sync::Mutex;

pub struct BackendConfig {
    pub config: GlobalConfig,
    pub route: SessionActivationRoute,
    pub abort: AbortSignal,
}

type LocalWorker = Arc<Mutex<Option<LocalWorkerSupervisor>>>;

/// One backend per broker cluster, shared by every export on that cluster.
pub struct Backend {
    pub runner: Arc<Runner>,
    pub store: Arc<A2aStore>,
    pub config: GlobalConfig,
    pub route: SessionActivationRoute,
    pub abort: AbortSignal,
    local_worker: Option<LocalWorker>,
    config_dir: Option<std::path::PathBuf>,
    admissions: parking_lot::Mutex<HashMap<String, Weak<Mutex<()>>>>,
}
impl Backend {
    pub fn new(runner: Arc<Runner>, store: Arc<A2aStore>, settings: BackendConfig) -> Self {
        let BackendConfig {
            config,
            route,
            abort,
        } = settings;
        Self {
            runner,
            store,
            config,
            route,
            abort,
            local_worker: None,
            config_dir: None,
            admissions: Default::default(),
        }
    }
    pub(crate) fn start_supervision(&self, exports: &[crate::exports::Export]) {
        let cluster = self.config.read().nats_routing.clone();
        let exports = exports
            .iter()
            .filter(|export| match &cluster {
                harnx_runtime::config::NatsRouting::Cluster(name) => {
                    export.cluster.as_ref() == Some(name)
                }
                _ => {
                    export
                        .cluster
                        .as_deref()
                        .unwrap_or(harnx_runtime::config::LOCAL_CLUSTER_KEY)
                        == harnx_runtime::config::LOCAL_CLUSTER_KEY
                }
            })
            .cloned()
            .collect();
        self.runner
            .start_supervision(crate::runner::SupervisionConfig {
                exports,
                config: self.config.clone(),
                route: self.route.clone(),
                abort: self.abort.clone(),
            });
    }
    pub(crate) fn with_local_worker(
        mut self,
        worker: LocalWorker,
        config_dir: Option<&std::path::Path>,
    ) -> Self {
        self.local_worker = Some(worker);
        self.config_dir = config_dir.map(std::path::Path::to_path_buf);
        self
    }
    pub(super) async fn session_route(&self) -> anyhow::Result<SessionActivationRoute> {
        match &self.local_worker {
            Some(worker) => {
                activation_route_for_cluster_with_config_dir(
                    harnx_runtime::config::LOCAL_CLUSTER_KEY,
                    worker,
                    self.abort.clone(),
                    self.config_dir.as_deref(),
                )
                .await
            }
            None => Ok(self.route.clone()),
        }
    }
    pub(super) fn gate(&self, key: String) -> Arc<Mutex<()>> {
        let mut gates = self.admissions.lock();
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(Mutex::new(()));
        gates.insert(key, Arc::downgrade(&gate));
        gate
    }
}
