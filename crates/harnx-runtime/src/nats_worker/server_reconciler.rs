//! Keep exactly the tool servers the active sessions need running.
//!
//! Sessions come and go, and different agents want different servers.
//! Starting every configured server up front costs a process per server for a
//! session that may use only one or two of them; stopping one the instant its
//! session ends restarts it moments later when the next session wants it. So:
//! reference count servers by [`ToolServerConfig::name`] across active
//! sessions, and let a server linger after its count hits zero before it is
//! actually stopped.

use super::daemon::{install_activation_agent, WorkerDaemonConfig};
use super::daemon_background::{configured_worker_services, should_start_tool_servers};
use super::tool_supervisor::{ToolServerStartConfig, ToolServerSupervisor};
use crate::config::{resolve_local_nats_server_config, GlobalConfig, ToolServerConfig};
use anyhow::Context;
use async_trait::async_trait;
use harnx_core::instance::ServerScope;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{watch, Mutex};

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

/// One server's status in `ServerReconciler::state`.
enum Slot {
    /// Believed to be a live, registered process. `idle_since` is set the
    /// moment `users` drops to empty and cleared the moment a new user
    /// joins.
    Running {
        users: HashSet<String>,
        idle_since: Option<Instant>,
    },
    /// `sweep` has begun tearing this name down and is awaiting
    /// `ServerLauncher::stop` outside the lock. Nothing may turn this back
    /// into `Running` in place — the process really is on its way out — so a
    /// session that wants this name while it's `Stopping` waits for `done`
    /// to resolve and then starts fresh (see `claim_or_wait`). `sweep`
    /// guarantees the entry is removed under the same lock before it lets
    /// `done` resolve, so a waiter that wakes and re-checks always finds
    /// `None`, never a second `Stopping`.
    ///
    /// This variant is what closes the race a same-tick `stop` used to hide:
    /// once `stop` started actually awaiting process exit and KV
    /// deregistration instead of dropping a `HashMap` entry, the window
    /// between "teardown begins" and "entry removed" stopped being
    /// negligible, and a session arriving in that window would join an
    /// entry `sweep` was about to delete out from under it — ending up with
    /// no server and no error.
    Stopping { done: watch::Receiver<()> },
}

/// Reference-counts tool servers by [`ToolServerConfig::name`] across the
/// sessions that requested them, starting a server on its first user and
/// stopping it `linger` after its last user goes away.
pub struct ServerReconciler {
    launcher: Arc<dyn ServerLauncher>,
    state: Mutex<HashMap<String, Slot>>,
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
    /// session is reused, not restarted. A server mid-teardown is waited out
    /// and then started fresh — see `claim_or_wait`.
    ///
    /// Split into [`Self::claim_users`] (registration) and
    /// [`Self::start_claimed`] (the slow part) so a caller that wants to bound
    /// only the slow part — see `WorkerRuntime::start_session_tool_servers`,
    /// which times out and detaches `start_claimed` into the background but
    /// must not do that to `claim_users` — can call them separately instead.
    pub async fn session_started(&self, session_id: &str, servers: Vec<ToolServerConfig>) {
        let to_start = self.claim_users(session_id, servers).await;
        self.start_claimed(to_start).await;
    }

    /// Register `session_id` as a user of each of `servers`, returning the
    /// subset that actually need starting (first user of a fresh entry, or
    /// waited out a teardown and must start fresh).
    ///
    /// Always run this to completion before any call to [`Self::session_ended`]
    /// for the same `session_id` — once it returns, `session_ended` is
    /// guaranteed to see (and release) every registration it made, even if
    /// the caller then abandons [`Self::start_claimed`] to a timeout. A
    /// caller that instead spawns this whole registration step into the
    /// background and times out on it can lose the race: `session_ended`
    /// runs first, sees nothing to release, and the registration that lands
    /// afterward pins the server as a "user" that will never end.
    pub async fn claim_users(
        &self,
        session_id: &str,
        servers: Vec<ToolServerConfig>,
    ) -> Vec<ToolServerConfig> {
        // Claim each name concurrently, not one at a time: a name that is
        // mid-teardown can make `claim_or_wait` wait a while, and that must
        // not delay a different server this same session also asked for.
        let claims = servers
            .into_iter()
            .map(|server| self.claim_or_wait(session_id, server));
        futures_util::future::join_all(claims)
            .await
            .into_iter()
            .flatten()
            .collect()
    }

    /// Start every server [`Self::claim_users`] said needs it, then sweep.
    ///
    /// Safe to time out or abandon into the background: the only effect of
    /// skipping this is that `to_start`'s servers stay unregistered for this
    /// activation (degraded tools, not a correctness bug), because the user
    /// registration this depends on already landed in `claim_users`.
    pub async fn start_claimed(&self, to_start: Vec<ToolServerConfig>) {
        // Never hold `state` across a launcher await: starting a process and
        // waiting on its registration can take seconds, and holding the lock
        // here would serialize every session activation behind whichever
        // server is slowest to come up.
        //
        // Start them concurrently rather than one at a time: sequential starts
        // sum each server's own startup timeout (three servers, one missing a
        // binary, is 3x that timeout), which the caller must fit inside the
        // activation's own ack window.
        let starts = to_start
            .into_iter()
            .map(|server| self.start_or_forget(server));
        futures_util::future::join_all(starts).await;
        self.sweep().await;
    }

    /// Register `session_id` as a user of `server`, first waiting out any
    /// teardown already in flight for the same name. Returns `Some(server)`
    /// exactly when this call must start it itself: either it is the first
    /// user of a freshly inserted entry, or the entry it waited on finished
    /// tearing down, in which case it is now gone and needs a fresh start
    /// same as a first user would. Either way the caller starts it outside
    /// the lock.
    async fn claim_or_wait(
        &self,
        session_id: &str,
        server: ToolServerConfig,
    ) -> Option<ToolServerConfig> {
        loop {
            let mut waiter = {
                let mut state = self.state.lock().await;
                match state.get_mut(&server.name) {
                    Some(Slot::Running { users, idle_since }) => {
                        users.insert(session_id.to_string());
                        *idle_since = None;
                        return None;
                    }
                    Some(Slot::Stopping { done }) => done.clone(),
                    None => {
                        state.insert(
                            server.name.clone(),
                            Slot::Running {
                                users: HashSet::from([session_id.to_string()]),
                                idle_since: None,
                            },
                        );
                        return Some(server);
                    }
                }
            };
            // A `watch` channel, not a `Notify`: `sweep` sends (or, on its
            // last iteration, just drops the sender) only after it has
            // already removed the entry under the same lock, but that send
            // can happen before this task ever gets here to await it. A
            // `Notify::notify_waiters` sent in that window would be lost;
            // `watch`'s "changed since this receiver was cloned" semantics
            // cannot miss it.
            let _ = waiter.changed().await;
        }
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
            for slot in state.values_mut() {
                if let Slot::Running { users, idle_since } = slot {
                    if users.remove(session_id) && users.is_empty() {
                        *idle_since = Some(Instant::now());
                    }
                }
            }
        }
        self.sweep().await;
    }

    /// Stop servers whose linger window has passed with no users left.
    ///
    /// Moves each expired entry to `Slot::Stopping` before releasing the
    /// lock and awaiting `stop`, rather than leaving the `Running` entry in
    /// place until `stop` returns: a session that asks for this name during
    /// that window must not be handed a place in an entry that is already on
    /// its way out (see `claim_or_wait`). The transition happens under the
    /// same lock as the expiry check, so two concurrent sweeps can't both
    /// pick the same name and call `stop` on it twice.
    async fn sweep(&self) {
        let expired: Vec<(String, watch::Sender<()>)> = {
            let mut state = self.state.lock().await;
            let names: Vec<String> = state
                .iter()
                .filter_map(|(name, slot)| match slot {
                    // `>=` rather than `>`: with `linger` at zero this must
                    // fire on the same call that dropped the last user.
                    Slot::Running { idle_since, .. }
                        if idle_since.is_some_and(|since| since.elapsed() >= self.linger) =>
                    {
                        Some(name.clone())
                    }
                    _ => None,
                })
                .collect();
            names
                .into_iter()
                .map(|name| {
                    let (done_tx, done_rx) = watch::channel(());
                    state.insert(name.clone(), Slot::Stopping { done: done_rx });
                    (name, done_tx)
                })
                .collect()
        };
        for (name, done) in expired {
            self.launcher.stop(&name).await;
            self.state.lock().await.remove(&name);
            // Wakes any `claim_or_wait` loop parked on this name's `done`
            // clone: dropping the sender closes the channel, and `changed`
            // reports that the same as a value change. The entry is already
            // gone by the time this runs, so whoever wakes retries into a
            // fresh start rather than a second `Stopping`.
            drop(done);
        }
    }

    /// Sorted names of servers currently believed running, for tests. A name
    /// mid-teardown (`Slot::Stopping`) is deliberately excluded — it is not
    /// running, it is on its way out.
    pub async fn running(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .state
            .lock()
            .await
            .iter()
            .filter_map(|(name, slot)| match slot {
                Slot::Running { .. } => Some(name.clone()),
                Slot::Stopping { .. } => None,
            })
            .collect();
        names.sort();
        names
    }
}

/// Production [`ServerLauncher`]: wraps [`ToolServerSupervisor`]. `stop`
/// explicitly calls `ToolServerSupervisor::shutdown` rather than relying on a
/// bare drop, so the child's registration and any co-located hook
/// registrations are actually removed before the process is gone.
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
        // `shutdown` (not a bare drop) is what actually deregisters: it waits
        // for each child to exit and lets its monitor's own exit path remove
        // the tool registration, then shuts down co-located hook supervisors,
        // which have no `Drop` at all.
        if let Some(supervisor) = self.running.lock().await.remove(config_name) {
            supervisor.shutdown().await;
        }
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
