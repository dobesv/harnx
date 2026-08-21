//! Worker daemon and session activation.

use super::daemon_background::launch_worker_services;
use super::daemon_runtime::WorkerRuntime;
use crate::config::{GlobalConfig, LOCAL_CLUSTER_KEY};
use crate::nats_lease::NatsSessionLease;
use crate::nats_session_index;
use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer::pull};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::Mutex;

pub use super::activation::{
    new_remote_session_id, new_worker_id, SessionActivate, SessionActivationRoute,
};
use super::activation_transport::ensure_activation_consumer;
pub(super) use super::activation_transport::spawn_readiness_publisher;
pub use super::activation_transport::{
    notify_subject, publish_session_activate, publish_targeted_session_activate,
    targeted_consumer_name, targeted_notify_subject, targeted_worker_ready_subject,
    validate_worker_id, worker_ready_subject, LocalWorkerTarget,
};
pub use super::daemon_config::{
    resolve_worker_scope, NatsConnectionSource, WorkerActivationMode, WorkerDaemonConfig,
};

pub(crate) fn should_append_control_log_entry(lease: &NatsSessionLease) -> bool {
    lease.is_held()
}

pub(super) struct WorkerStartup {
    pub(super) jetstream: jetstream::Context,
    pub(super) client: async_nats::Client,
    pub(super) consumer: jetstream::consumer::Consumer<pull::Config>,
    /// JetStream replica count for the daemon connection, resolved once here so
    /// every bucket this worker creates (leases, session index, tool/hook
    /// registries) agrees on it instead of drifting to a per-site default.
    pub(super) replicas: usize,
    pub(super) identity: crate::worker_identity::WorkerReadiness,
}

async fn prepare_worker_startup(
    config: &GlobalConfig,
    daemon: &WorkerDaemonConfig,
) -> Result<WorkerStartup> {
    validate_worker_daemon(daemon)?;
    let identity =
        crate::worker_identity::WorkerReadiness::current(&daemon.session_scope, &daemon.worker_id);
    let (jetstream, client, replicas) = connect_worker(config, daemon).await?;
    let consumer = ensure_activation_consumer(&jetstream, daemon).await?;
    Ok(WorkerStartup {
        jetstream,
        client,
        consumer,
        replicas,
        identity,
    })
}

fn validate_worker_daemon(daemon: &WorkerDaemonConfig) -> Result<()> {
    if daemon.activation_mode == WorkerActivationMode::WorkerTargeted {
        validate_worker_id(&daemon.worker_id)?;
        anyhow::ensure!(
            daemon.session_scope == LOCAL_CLUSTER_KEY,
            "targeted workers currently require session scope {LOCAL_CLUSTER_KEY}"
        );
        anyhow::ensure!(
            daemon.connection == NatsConnectionSource::LocalEnvironment,
            "targeted local workers require the local NATS environment handoff"
        );
        anyhow::ensure!(
            daemon.manage_servers,
            "targeted local workers require managed tool and hook servers"
        );
    }
    Ok(())
}

async fn connect_worker(
    config: &GlobalConfig,
    daemon: &WorkerDaemonConfig,
) -> Result<(jetstream::Context, async_nats::Client, usize)> {
    let (jetstream, client, replicas) = {
        let cfg = config.read().clone();
        let connection_key = daemon.connection_key();
        let jetstream = cfg.nats_jetstream(connection_key).await?;
        let client = cfg.nats_client(connection_key).await?;
        let replicas = cfg.resolve_nats_server(connection_key).await?.replicas;
        (jetstream, client, replicas.unwrap_or(1))
    };
    Ok((jetstream, client, replicas))
}

pub(super) async fn optional_session_index(
    jetstream: &jetstream::Context,
    replicas: usize,
) -> Option<async_nats::jetstream::kv::Store> {
    match nats_session_index::ensure_index_bucket(jetstream, replicas).await {
        Ok(store) => Some(store),
        Err(error) => {
            log::warn!(
                "session index disabled: failed to ensure harnx_sessions index bucket: {:#}",
                error
            );
            None
        }
    }
}

/// Install the activation's agent into the per-session config.
///
/// Mirrors the local `use_agent_by_name` flow: file-backed variable defaults
/// (`variables: [{name, path}]`) must be read off disk and folded into the
/// agent's shared variables before anything renders the prompt template, or
/// an agent like `pantheon/sisyphus` fails with an undefined value on its
/// first render.
///
/// An agent the worker simply does not have is not fatal — the session falls
/// back to the worker's own configuration, as it has always done. Anything
/// else is: an agent whose file is present but unreadable, unparseable, or
/// names a model the worker cannot resolve must not silently answer as some
/// other agent.
///
/// `pub(super)`: also used by `server_reconciler::tool_servers_for_activation`
/// to resolve which servers a session's agent needs, on a throwaway config
/// clone, before the real per-session config below is built.
pub(super) fn install_activation_agent(per_session: &GlobalConfig, agent_name: &str) -> Result<()> {
    let retrieved = per_session.read().retrieve_agent(agent_name);
    let mut agent = match retrieved {
        Ok(agent) => agent,
        // No file and no built-in by this name: nothing to load.
        Err(error) if !crate::config::Config::agent_file(agent_name).exists() => {
            log::warn!(
                "activation agent '{agent_name}' not available, using worker config: {error:#}"
            );
            return Ok(());
        }
        Err(error) => return Err(error).context(format!("load agent '{agent_name}'")),
    };
    crate::config::agent::resolve_file_defaults(&mut agent)
        .with_context(|| format!("resolve file-backed variables for agent '{agent_name}'"))?;
    let mut cfg = per_session.write();
    cfg.use_agent_obj(agent)
        .with_context(|| format!("activate agent '{agent_name}'"))?;
    cfg.require_agent_shared_variables()
        .with_context(|| format!("initialize variables for agent '{agent_name}'"))
}

/// Run a worker daemon: pull `SessionActivate` notifications, claim each via a
/// KV lease, and execute the session (exactly one worker per session).
pub async fn run_worker_daemon(
    config: GlobalConfig,
    mut daemon: WorkerDaemonConfig,
    call_fn: Option<crate::agent_loop::AgentCallFn>,
) -> Result<()> {
    let instance_id = resolve_worker_scope(daemon.manage_servers)?;
    let startup = prepare_worker_startup(&config, &daemon).await?;
    log::info!(
        "serving under server_scope='{}' session_scope={} worker_id={} pid={} build={} activation={:?}",
        instance_id.as_str(),
        startup.identity.session_scope,
        startup.identity.worker_id,
        startup.identity.pid,
        startup.identity.build,
        daemon.activation_mode,
    );
    // The daemon config has no cluster to resolve against yet, so its lease
    // config carries the bare default; now that the connection source is
    // resolved, the lease bucket agrees with everything else this worker
    // creates instead of always sitting at the default.
    daemon.lease.replicas = startup.replicas;
    let services = launch_worker_services(&config, &daemon, &startup, &instance_id).await?;
    let runtime = Arc::new(WorkerRuntime {
        config,
        instance_id,
        _background_services: services.background,
        tools_attempted: services.tools_attempted,
        server_reconciler: services.server_reconciler,
        cluster: daemon.connection_key().to_string(),
        activation_route: daemon.activation_route(),
        activation_mode: daemon.activation_mode,
        manage_servers: daemon.manage_servers,
        worker_id: daemon.worker_id.clone(),
        identity: startup.identity.clone(),
        lease: daemon.lease,
        jetstream: startup.jetstream,
        session_index: services.session_index,
        client: startup.client,
        call_fn,
        generation: AtomicU64::new(1),
        active: Mutex::new(HashMap::new()),
    });

    let mut messages = startup
        .consumer
        .messages()
        .await
        .context("worker notify message stream")?;
    while let Some(message) = messages.next().await {
        let message = message.context("receive activation")?;
        if let Err(error) = runtime.handle_activation(message).await {
            log::warn!("worker activation handling failed: {error:#}");
        }
    }
    Ok(())
}
