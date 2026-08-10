//! Keep exactly the tool servers the active sessions need running.
//!
//! Sessions come and go, and different agents want different servers.
//! Starting every configured server up front costs a process per server for a
//! session that may use only one or two of them; stopping one the instant its
//! session ends restarts it moments later when the next session wants it. So:
//! reference count servers by [`ToolServerConfig::name`] across active
//! sessions, and let a server linger after its count hits zero before it is
//! actually stopped.

use super::daemon::{
    configured_worker_services, install_activation_agent, should_start_tool_servers,
    WorkerDaemonConfig,
};
use super::tool_supervisor::{ToolServerStartConfig, ToolServerSupervisor};
use crate::config::{resolve_local_nats_server_config, GlobalConfig, ToolServerConfig};
use anyhow::Context;
use async_trait::async_trait;
use harnx_core::instance::ServerScope;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// How long an idle tool server lingers with no sessions using it before the
/// reconciler actually stops it. Long enough that back-to-back sessions (a
/// front-end restarting a session, a new one starting moments after the last
/// one ended) reuse the running process instead of paying startup and
/// registration cost again.
const TOOL_SERVER_LINGER: Duration = Duration::from_secs(60);

/// Starting and stopping the actual processes, behind a trait so the
/// reference-counting rules can be tested without a broker or child
/// processes.
#[async_trait]
pub trait ServerLauncher: Send + Sync {
    async fn start(&self, server: &ToolServerConfig) -> anyhow::Result<()>;
    async fn stop(&self, config_name: &str);
}

/// One server's current users and, once none remain, when the last one left.
struct Running {
    users: HashSet<String>,
    idle_since: Option<Instant>,
}

/// Add `session_id` as a user of `server` in `state`. Queues `server` onto
/// `to_start` exactly when this is its first user (no entry existed yet) —
/// pulled out of `session_started`'s loop so that loop body is a single call
/// with no conditional of its own.
fn add_user_or_queue_start(
    state: &mut HashMap<String, Running>,
    to_start: &mut Vec<ToolServerConfig>,
    session_id: &str,
    server: ToolServerConfig,
) {
    match state.get_mut(&server.name) {
        Some(entry) => {
            entry.users.insert(session_id.to_string());
            entry.idle_since = None;
        }
        None => {
            state.insert(
                server.name.clone(),
                Running {
                    users: HashSet::from([session_id.to_string()]),
                    idle_since: None,
                },
            );
            to_start.push(server);
        }
    }
}

/// Reference-counts tool servers by [`ToolServerConfig::name`] across the
/// sessions that requested them, starting a server on its first user and
/// stopping it `linger` after its last user goes away.
pub struct ServerReconciler {
    launcher: Arc<dyn ServerLauncher>,
    state: Mutex<HashMap<String, Running>>,
    linger: Duration,
}

impl ServerReconciler {
    pub fn new(launcher: Arc<dyn ServerLauncher>, linger: Duration) -> Self {
        Self {
            launcher,
            state: Mutex::new(HashMap::new()),
            linger,
        }
    }

    /// Register `session_id` as a user of each of `servers`, starting any
    /// that have no other user yet. A server already running for another
    /// session is reused, not restarted.
    pub async fn session_started(&self, session_id: &str, servers: Vec<ToolServerConfig>) {
        let mut to_start = Vec::new();
        {
            let mut state = self.state.lock().await;
            for server in servers {
                add_user_or_queue_start(&mut state, &mut to_start, session_id, server);
            }
        }
        // Never hold `state` across a launcher await: starting a process and
        // waiting on its registration can take seconds, and holding the lock
        // here would serialize every session activation behind whichever
        // server is slowest to come up.
        for server in to_start {
            self.start_or_forget(server).await;
        }
        self.sweep().await;
    }

    /// Start `server` and, on failure, remove it from `state` rather than
    /// leave it counted as running.
    ///
    /// There is deliberately no background retry loop for a failed start:
    /// on-demand starting means the next session that wants this server
    /// tries fresh, since it isn't in `state`. The trade-off is real — a
    /// session already running when a broken server gets fixed will not see
    /// it come back; only a later activation will. The old worker-wide retry
    /// loop this replaced covered that case, at the cost of retrying forever
    /// in the background for a server no active session wanted.
    async fn start_or_forget(&self, server: ToolServerConfig) {
        if let Err(error) = self.launcher.start(&server).await {
            log::warn!("tool server '{}' unavailable: {error:#}", server.name);
            self.state.lock().await.remove(&server.name);
        }
    }

    /// Remove `session_id` from every server's user set. A server that drops
    /// to zero users starts its linger window rather than stopping
    /// immediately.
    pub async fn session_ended(&self, session_id: &str) {
        {
            let mut state = self.state.lock().await;
            for entry in state.values_mut() {
                if entry.users.remove(session_id) && entry.users.is_empty() {
                    entry.idle_since = Some(Instant::now());
                }
            }
        }
        self.sweep().await;
    }

    /// Stop servers whose linger window has passed with no users left.
    async fn sweep(&self) {
        let expired: Vec<String> = {
            let state = self.state.lock().await;
            state
                .iter()
                .filter(|(_, entry)| {
                    // `>=` rather than `>`: with `linger` at zero this must
                    // fire on the same call that dropped the last user.
                    entry
                        .idle_since
                        .is_some_and(|since| since.elapsed() >= self.linger)
                })
                .map(|(name, _)| name.clone())
                .collect()
        };
        for name in expired {
            self.launcher.stop(&name).await;
            self.state.lock().await.remove(&name);
        }
    }

    /// Sorted names of servers currently tracked as running, for tests.
    pub async fn running(&self) -> Vec<String> {
        let mut names: Vec<String> = self.state.lock().await.keys().cloned().collect();
        names.sort();
        names
    }
}

/// Production [`ServerLauncher`]: wraps [`ToolServerSupervisor`], which kills
/// its children on drop, so removing a server from `running` here is enough
/// to stop it.
pub struct SupervisorLauncher {
    start: ToolServerStartConfig,
    running: Mutex<HashMap<String, ToolServerSupervisor>>,
}

impl SupervisorLauncher {
    pub fn new(start: ToolServerStartConfig) -> Self {
        Self {
            start,
            running: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl ServerLauncher for SupervisorLauncher {
    async fn start(&self, server: &ToolServerConfig) -> anyhow::Result<()> {
        let supervisor =
            ToolServerSupervisor::start_local(self.start.clone(), std::slice::from_ref(server))
                .await?;
        // `start_local` treats a server that never registers as a soft
        // failure (it logs and returns `Ok` with the server recorded as
        // unregistered) so a batch of many servers isn't all-or-nothing. That
        // collapsing is wrong here: the reconciler needs to know this one
        // server failed so it doesn't count it as running.
        let unregistered = supervisor.unregistered_servers();
        if !unregistered.is_empty() {
            // Dropping the supervisor kills whatever child it did spawn.
            anyhow::bail!(
                "tool server '{}' did not register: {}",
                server.name,
                unregistered.join(", ")
            );
        }
        self.running
            .lock()
            .await
            .insert(server.name.clone(), supervisor);
        Ok(())
    }

    async fn stop(&self, config_name: &str) {
        self.running.lock().await.remove(config_name);
    }
}

/// Every enabled tool server across the whole config, regardless of which
/// agent (if any) uses it.
///
/// The gate for building a [`ServerReconciler`] at all: a superset here is
/// safe (a session only ever asks the reconciler to start the subset its own
/// agent uses; see [`tool_servers_for_activation`]), but an empty result must
/// stop this worker from resolving a broker address for nothing, same as
/// [`should_start_tool_servers`].
pub(super) fn all_enabled_tool_servers(config: &GlobalConfig) -> Vec<ToolServerConfig> {
    config
        .read()
        .tool_servers
        .iter()
        .filter(|server| server.enabled)
        .cloned()
        .collect()
}

/// Build the reconciler that starts and stops this worker's own tool servers
/// as sessions activate and finish, or `None` when there is nothing this
/// worker could ever be asked to start.
///
/// Resolves the broker address once, eagerly, here at startup — exactly like
/// the old one-shot batch start used to — which is safe only because the
/// caller gates this on [`should_start_tool_servers`] over the *unfiltered*
/// server list first: nothing configured anywhere means no resolution, same
/// invariant the old one-shot start honored per agent.
pub(super) async fn build_server_reconciler(
    daemon: &WorkerDaemonConfig,
    client: async_nats::Client,
    instance_id: &ServerScope,
    all_servers: &[ToolServerConfig],
) -> Option<Arc<ServerReconciler>> {
    if !should_start_tool_servers(daemon.manage_servers, all_servers) {
        return None;
    }
    let result = async {
        let server = resolve_local_nats_server_config().await?;
        let token = server
            .token
            .as_deref()
            .context("local NATS tool servers require HARNX_NATS_TOKEN")?;
        let start = ToolServerStartConfig::new(client, instance_id.clone(), &server.url, token)
            .with_replicas(server.replicas)
            .with_tls(&harnx_nats_common::connect::NatsEndpoint::from(&server));
        anyhow::Result::<SupervisorLauncher>::Ok(SupervisorLauncher::new(start))
    }
    .await;
    match result {
        Ok(launcher) => Some(Arc::new(ServerReconciler::new(
            Arc::new(launcher),
            TOOL_SERVER_LINGER,
        ))),
        Err(error) => {
            log::warn!("local NATS tool servers disabled; continuing with stdio tools: {error:#}");
            None
        }
    }
}

/// Tool servers the session's own agent needs, resolved the same way
/// [`install_activation_agent`] resolves the agent for real execution: loaded
/// from the worker's config by name, falling back to the worker's own
/// configuration when the named agent is missing or fails to load. Runs
/// against a throwaway clone of the worker's config — discarded once the
/// server list is read off it — so it has no effect on the shared config or
/// on the real per-session config `execute_session` builds separately once
/// the turn actually runs.
pub(super) fn tool_servers_for_activation(
    config: &GlobalConfig,
    agent_name: &str,
) -> Vec<ToolServerConfig> {
    let scratch: GlobalConfig = Arc::new(parking_lot::RwLock::new(config.read().clone()));
    if let Err(error) = install_activation_agent(&scratch, agent_name) {
        log::warn!(
            "could not resolve agent '{agent_name}' for tool-server selection, \
             falling back to the worker's own configuration: {error:#}"
        );
    }
    configured_worker_services(&scratch).0
}
