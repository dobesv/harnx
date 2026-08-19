//! Worker daemon and session activation.

use super::daemon_background::launch_worker_services;
use super::daemon_runtime::WorkerRuntime;
use crate::config::GlobalConfig;
use crate::nats_lease::{NatsLeaseConfig, NatsSessionLease};
use crate::nats_session_index;
use anyhow::{Context, Result};
use async_nats::header::{HeaderValue, NATS_MESSAGE_ID};
use async_nats::jetstream::{
    self,
    consumer::{pull, DeliverPolicy},
    stream::{Config as StreamConfig, RetentionPolicy, StorageType},
};
use chrono::Utc;
use futures_util::StreamExt;
use harnx_core::instance::{ServerScope, HARNX_SERVER_SCOPE};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// P2.1 worker daemon + SessionActivate dispatch
// ---------------------------------------------------------------------------

const WORK_NOTIFY_STREAM_PREFIX: &str = "WORK_NOTIFY_";
const WORK_NOTIFY_CONSUMER_PREFIX: &str = "worker-";
const WORK_NOTIFY_ACK_WAIT: Duration = Duration::from_secs(30);
const WORK_NOTIFY_INACTIVE_THRESHOLD: Duration = Duration::from_secs(60 * 60);

/// Configuration for a worker daemon instance.
#[derive(Debug, Clone)]
pub struct WorkerDaemonConfig {
    pub cluster: String,
    pub worker_id: String,
    pub lease: NatsLeaseConfig,
    /// Whether this worker launches its own tool and hook servers as child
    /// processes (and mints a scope for them) rather than discovering
    /// independently deployed ones under a configured scope.
    pub manage_servers: bool,
}

impl WorkerDaemonConfig {
    /// A worker that discovers independently deployed servers rather than
    /// launching its own. Callers that want the old all-in-one behavior use
    /// [`Self::managing`] instead.
    pub fn new(cluster: impl Into<String>, worker_id: impl Into<String>) -> Self {
        Self {
            cluster: cluster.into(),
            worker_id: worker_id.into(),
            lease: NatsLeaseConfig::default(),
            manage_servers: false,
        }
    }

    /// A worker that launches its own tool and hook servers as child processes.
    pub fn managing(cluster: impl Into<String>, worker_id: impl Into<String>) -> Self {
        Self {
            manage_servers: true,
            ..Self::new(cluster, worker_id)
        }
    }
}

/// Resolve the scope this worker addresses servers under.
///
/// A worker that manages its own servers mints a fresh scope for them, same
/// as before this flag existed. A worker pointed at independently deployed
/// servers has no scope of its own to mint — it must be told, via
/// `HARNX_SERVER_SCOPE`, which one those servers registered under.
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

/// Activation request published by a client to wake/claim a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionActivate {
    pub session_id: String,
    pub agent: String,
    pub epoch: String,
}

impl SessionActivate {
    pub fn new(session_id: impl Into<String>, agent: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            agent: agent.into(),
            epoch: Utc::now().to_rfc3339(),
        }
    }

    /// Dedup id for the notify stream (`Nats-Msg-Id`): session + epoch.
    pub fn msg_id(&self) -> String {
        format!("{}:{}", self.session_id, self.epoch)
    }
}

/// Generate a fresh remote session id (uuid v7, time-ordered).
pub fn new_remote_session_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Notify subject for a cluster's session activations.
pub fn notify_subject(cluster: &str) -> String {
    format!("cluster.{cluster}.sessions.notify")
}

/// Core-NATS subject used by workers to announce that their activation pull
/// consumer exists and can receive [`SessionActivate`] notifications.
pub fn worker_ready_subject(cluster: &str) -> String {
    format!("cluster.{cluster}.worker.ready")
}

fn notify_stream_name(cluster: &str) -> String {
    format!(
        "{WORK_NOTIFY_STREAM_PREFIX}{}",
        sanitize_name_component(cluster)
    )
}

fn durable_consumer_name(worker_id: &str) -> String {
    format!(
        "{WORK_NOTIFY_CONSUMER_PREFIX}{}",
        sanitize_name_component(worker_id)
    )
}

pub(crate) fn should_append_control_log_entry(lease: &NatsSessionLease) -> bool {
    lease.is_held()
}

fn sanitize_name_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if is_valid_name_component_char(ch) {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn is_valid_name_component_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')
}

async fn ensure_notify_stream(
    jetstream: &jetstream::Context,
    cluster: &str,
    subject: &str,
) -> Result<jetstream::stream::Stream> {
    let name = notify_stream_name(cluster);
    if let Ok(stream) = jetstream.get_stream(&name).await {
        return Ok(stream);
    }
    match jetstream
        .create_stream(StreamConfig {
            name: name.clone(),
            description: Some("session activation work queue".to_string()),
            subjects: vec![subject.to_string()],
            retention: RetentionPolicy::WorkQueue,
            storage: StorageType::File,
            ..Default::default()
        })
        .await
    {
        Ok(stream) => Ok(stream),
        Err(_) => jetstream
            .get_stream(&name)
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("Failed to create notify stream for cluster '{cluster}'")),
    }
}

/// Publish a `SessionActivate` notification (idempotent via `Nats-Msg-Id`).
pub async fn publish_session_activate(
    jetstream: &jetstream::Context,
    cluster: &str,
    activation: &SessionActivate,
) -> Result<u64> {
    let subject = notify_subject(cluster);
    ensure_notify_stream(jetstream, cluster, &subject).await?;
    let payload = serde_json::to_vec(activation).context("serialize SessionActivate")?;
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(NATS_MESSAGE_ID, HeaderValue::from(activation.msg_id()));
    let ack = jetstream
        .publish_with_headers(subject, headers, payload.into())
        .await
        .context("publish SessionActivate")?
        .await
        .context("ack SessionActivate")?;
    Ok(ack.sequence)
}

pub(super) struct WorkerStartup {
    pub(super) jetstream: jetstream::Context,
    pub(super) client: async_nats::Client,
    pub(super) consumer: jetstream::consumer::Consumer<pull::Config>,
    /// JetStream replica count for `daemon.cluster`, resolved once here so
    /// every bucket this worker creates (leases, session index, tool/hook
    /// registries) agrees on it instead of drifting to a per-site default.
    pub(super) replicas: usize,
    pub(super) identity: crate::worker_identity::WorkerIdentity,
}

async fn prepare_worker_startup(
    config: &GlobalConfig,
    daemon: &WorkerDaemonConfig,
) -> Result<WorkerStartup> {
    let identity = crate::worker_identity::WorkerIdentity::current(&daemon.worker_id).await?;
    let (jetstream, client, replicas) = {
        let cfg = config.read().clone();
        let jetstream = cfg.nats_jetstream(&daemon.cluster).await?;
        let client = cfg.nats_client(&daemon.cluster).await?;
        let replicas = cfg.resolve_nats_server(&daemon.cluster).await?.replicas;
        (jetstream, client, replicas.unwrap_or(1))
    };
    let subject = notify_subject(&daemon.cluster);
    let stream = ensure_notify_stream(&jetstream, &daemon.cluster, &subject).await?;
    let consumer_name = durable_consumer_name(&daemon.worker_id);
    let consumer = stream
        .get_or_create_consumer(
            &consumer_name,
            pull::Config {
                durable_name: Some(consumer_name.clone()),
                deliver_policy: DeliverPolicy::All,
                ack_wait: WORK_NOTIFY_ACK_WAIT,
                filter_subject: subject,
                inactive_threshold: WORK_NOTIFY_INACTIVE_THRESHOLD,
                max_deliver: -1,
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("create worker consumer '{consumer_name}'"))?;
    Ok(WorkerStartup {
        jetstream,
        client,
        consumer,
        replicas,
        identity,
    })
}

pub(super) fn spawn_readiness_publisher(
    client: async_nats::Client,
    daemon: &WorkerDaemonConfig,
    identity: &crate::worker_identity::WorkerIdentity,
) -> Result<()> {
    let subject = worker_ready_subject(&daemon.cluster);
    let payload = identity.payload()?;
    tokio::spawn(async move {
        loop {
            if let Err(error) = client
                .publish(subject.clone(), payload.clone().into())
                .await
            {
                log::warn!("failed to publish worker readiness marker: {error}");
                return;
            }
            if let Err(error) = client.flush().await {
                log::warn!("failed to flush worker readiness marker: {error}");
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
    Ok(())
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
        "serving under scope '{}' worker_id={} pid={} build={} executable={} config={}",
        instance_id.as_str(),
        startup.identity.worker_id,
        startup.identity.pid,
        startup.identity.build,
        crate::worker_identity::short_fingerprint(&startup.identity.executable_fingerprint),
        crate::worker_identity::short_fingerprint(&startup.identity.config_fingerprint),
    );
    // `WorkerDaemonConfig::new` has no cluster to resolve against yet, so its
    // lease config carries the bare default; now that `daemon.cluster` is
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
        cluster: daemon.cluster.clone(),
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
