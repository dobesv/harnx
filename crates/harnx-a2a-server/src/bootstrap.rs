//! Configuration and worker lifetime for the HTTP process.

use crate::{
    exports::Export,
    handler::{Backend, BackendConfig},
    runner::Runner,
    store::A2aStore,
};
use anyhow::{Context, Result};
use harnx_core::{abort::AbortSignal, access_rules::AccessRules};
use harnx_runtime::{
    config::{Config, ConfigLock, NatsRouting, LOCAL_CLUSTER_KEY},
    local_orchestrator::{activation_route_for_cluster_with_config_dir, LocalWorkerSupervisor},
    nats_session_metadata::SessionMetadataStore,
};
use std::{collections::HashMap, path::Path, sync::Arc};
use tokio::sync::Mutex;

pub(crate) struct Bootstrap {
    pub backends: HashMap<String, Arc<Backend>>,
    // Keep the managed worker alive until admitted turns have settled.
    local_worker: Arc<Mutex<Option<LocalWorkerSupervisor>>>,
    abort: AbortSignal,
}
impl Drop for Bootstrap {
    fn drop(&mut self) {
        self.abort.set_ctrlc();
    }
}
impl Bootstrap {
    pub async fn new(
        exports: &[Export],
        config_dir: Option<&Path>,
        abort: AbortSignal,
        access_rules: Option<Arc<AccessRules>>,
    ) -> Result<Self> {
        let config_path = config_dir
            .map(|dir| dir.join("config.yaml"))
            .unwrap_or_else(Config::config_file);
        let mut config = Config::load_from_file(&config_path).context("load A2A runtime config")?;
        config.apply_frontend_nats_routing();
        let mut bootstrap = Self {
            backends: HashMap::new(),
            local_worker: Arc::new(Mutex::new(None)),
            abort,
        };
        for export in exports {
            let cluster = export.cluster.as_deref().unwrap_or(LOCAL_CLUSTER_KEY);
            if bootstrap.backends.contains_key(cluster) {
                continue;
            }
            let route = activation_route_for_cluster_with_config_dir(
                cluster,
                &bootstrap.local_worker,
                bootstrap.abort.clone(),
                config_dir,
            )
            .await?;
            let mut cluster_config = config.clone();
            cluster_config.nats_routing = if cluster == LOCAL_CLUSTER_KEY {
                NatsRouting::FrontendLocal
            } else {
                NatsRouting::Cluster(cluster.to_owned())
            };
            let replicas = if cluster == LOCAL_CLUSTER_KEY {
                1
            } else {
                cluster_config.nats_server(cluster)?.resolved_replicas()
            };
            let jetstream = cluster_config.nats_jetstream(cluster).await?;
            let store = Arc::new(A2aStore::new_with_access_rules(
                SessionMetadataStore::ensure(&jetstream, replicas).await?,
                access_rules.clone(),
            ));
            provision_coordination(&store).await?;
            let runner = Runner::new(store.clone());
            let backend = Backend::new(
                runner,
                store,
                BackendConfig {
                    config: Arc::new(ConfigLock::new(cluster_config)),
                    route,
                    abort: bootstrap.abort.clone(),
                },
            );
            backend.start_supervision(exports);
            let backend = if cluster == LOCAL_CLUSTER_KEY {
                backend.with_local_worker(bootstrap.local_worker.clone(), config_dir)
            } else {
                backend
            };
            bootstrap
                .backends
                .insert(cluster.to_owned(), Arc::new(backend));
        }
        Ok(bootstrap)
    }
}

async fn provision_coordination(store: &A2aStore) -> Result<()> {
    let metadata = store.metadata();
    let js = metadata.jetstream();
    let replicas = metadata.replicas();
    harnx_runtime::a2a_events::ensure(js, replicas).await?;
    let leases = harnx_runtime::nats_lease::ensure_lease_bucket(
        js,
        &harnx_runtime::nats_lease::NatsLeaseConfig {
            replicas,
            ..Default::default()
        },
    )
    .await?;
    anyhow::ensure!(
        leases.stream.cached_info().config.num_replicas == replicas,
        "A2A lease bucket replica count mismatch"
    );
    Ok(())
}
