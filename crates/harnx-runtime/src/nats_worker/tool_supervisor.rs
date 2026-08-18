use super::hook_supervisor::{HookServerStartConfig, HookServerSupervisor};
use super::process_manager::ChildProcessManager;
use super::tool_registry::{
    ensure_registry_bucket, log_registry_contents, remove_registrations_for_config,
    wait_for_registration, RegistrationWait, SupervisedProcesses, SupervisedServer,
};
use crate::config::{
    ToolServerConfig, HARNX_NATS_REPLICAS_ENV, HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV,
};
use crate::nats_tool_provider::NatsInFlightCalls;
use anyhow::{Context, Result};
use async_nats::jetstream::kv;
use harnx_core::event::{AgentEvent, NoticeEvent};
use harnx_core::instance::{ServerScope, HARNX_SERVER_SCOPE};
use harnx_core::sink::emit_agent_event;
use harnx_hooks::executor::HARNX_PACKAGE_DIR_ENV;
use harnx_nats_common::connect::{
    NatsEndpoint, HARNX_NATS_TLS_CA_ENV, HARNX_NATS_TLS_CERT_ENV, HARNX_NATS_TLS_ENV,
    HARNX_NATS_TLS_KEY_ENV,
};
use harnx_toolset::{server_identity_token, HARNX_SERVER_CONFIG, HARNX_SERVER_PACKAGE};
use harnx_toolset_server::registration_key;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const TOOL_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

/// Connection and identity shared by all tool servers spawned for one worker.
#[derive(Clone)]
pub struct ToolServerStartConfig {
    client: async_nats::Client,
    instance_id: ServerScope,
    nats_url: String,
    token: String,
    /// JetStream replica count for buckets this cluster's tool servers
    /// create. `None` means 1; see `docs/nats-ha.md`.
    replicas: Option<usize>,
    /// TLS/mTLS settings for `nats_url`, mirrored into spawned children's
    /// environment. `None`/absent means no TLS, same as the shared local
    /// broker this worker falls back to when no cluster config says otherwise.
    tls: Option<bool>,
    tls_cert: Option<String>,
    tls_key: Option<String>,
    tls_ca: Option<String>,
    /// Send tool-server output to this process's own stdout/stderr instead of
    /// the worker log. Set by foreground diagnostics, where routing a server's
    /// explanation of its own failure into a file is the opposite of useful.
    inherit_child_output: bool,
    process_manager: ChildProcessManager,
}

impl ToolServerStartConfig {
    pub fn new(
        client: async_nats::Client,
        instance_id: ServerScope,
        nats_url: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        Self {
            client,
            instance_id,
            nats_url: nats_url.into(),
            token: token.into(),
            replicas: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            inherit_child_output: false,
            process_manager: ChildProcessManager::new(),
        }
    }

    /// Show tool-server output on this process's stdio rather than the worker log.
    pub fn inheriting_child_output(mut self) -> Self {
        self.inherit_child_output = true;
        self
    }

    /// Set the JetStream replica count from the cluster this config connects to.
    pub fn with_replicas(mut self, replicas: Option<usize>) -> Self {
        self.replicas = replicas;
        self
    }

    /// Copy TLS/mTLS settings from `endpoint` (built from the cluster this
    /// config connects to) so spawned tool servers are told to use the same
    /// ones. Only `endpoint`'s TLS fields are read; url/token/replicas are
    /// already carried separately. Without this, a worker discovering over
    /// TLS spawns children that connect plaintext and can never reach the
    /// broker.
    pub fn with_tls(mut self, endpoint: &NatsEndpoint) -> Self {
        self.tls = endpoint.tls;
        self.tls_cert = endpoint.tls_cert.clone();
        self.tls_key = endpoint.tls_key.clone();
        self.tls_ca = endpoint.tls_ca.clone();
        self
    }

    fn tls_endpoint(&self) -> NatsEndpoint {
        NatsEndpoint {
            tls: self.tls,
            tls_cert: self.tls_cert.clone(),
            tls_key: self.tls_key.clone(),
            tls_ca: self.tls_ca.clone(),
            ..Default::default()
        }
    }

    fn hook_start_config(&self) -> HookServerStartConfig {
        HookServerStartConfig::new(
            self.client.clone(),
            self.instance_id.clone(),
            self.nats_url.clone(),
            self.token.clone(),
        )
        .with_process_manager(self.process_manager.clone())
        .with_replicas(self.replicas)
        .with_tls(&self.tls_endpoint())
    }
}

/// Owns local tool-server children for one worker process.
pub struct ToolServerSupervisor {
    _process_manager: ChildProcessManager,
    processes: SupervisedProcesses,
    tasks: Vec<JoinHandle<()>>,
    hook_supervisors: Vec<HookServerSupervisor>,
    /// Enabled servers that have not registered yet. Retried in the background
    /// by the worker, so this shrinks as late registrations land.
    unregistered: Vec<SupervisedServer>,
    /// Tells every monitor task to kill its child and let the monitor's own
    /// exit path (registration removal) run, instead of `shutdown` aborting
    /// past it. A `CancellationToken`, not a `Notify`: `notify_waiters` only
    /// wakes tasks already parked on `notified()` at the moment it's called,
    /// so a monitor that hasn't reached its wait yet would miss the signal
    /// and wait for its child to exit on its own — which, for a tool server,
    /// is never. Cancellation is durable state, not a point-in-time wakeup,
    /// so a monitor that checks it late still sees it. `Drop` still aborts
    /// `tasks` directly as a fallback for a supervisor that gets dropped
    /// without `shutdown` ever running.
    shutdown_signal: CancellationToken,
}

impl ToolServerSupervisor {
    /// Spawn enabled local tool servers and wait for their registrations.
    pub async fn start_local(
        config: ToolServerStartConfig,
        servers: &[ToolServerConfig],
    ) -> Result<Self> {
        Self::start_local_with_timeout(config, servers, TOOL_STARTUP_TIMEOUT).await
    }

    /// Same startup path with an explicit readiness timeout for tests and callers.
    pub async fn start_local_with_timeout(
        config: ToolServerStartConfig,
        servers: &[ToolServerConfig],
        readiness_timeout: Duration,
    ) -> Result<Self> {
        let processes = Arc::new(Mutex::new(HashMap::new()));
        let mut supervisor = Self {
            _process_manager: config.process_manager.clone(),
            processes: Arc::clone(&processes),
            tasks: Vec::new(),
            hook_supervisors: Vec::new(),
            shutdown_signal: CancellationToken::new(),
            unregistered: Vec::new(),
        };
        let enabled: Vec<_> = servers.iter().filter(|server| server.enabled).collect();
        if enabled.is_empty() {
            return Ok(supervisor);
        }
        supervisor
            .start_servers(&config, &enabled, readiness_timeout)
            .await;
        start_co_located_hooks(&mut supervisor, &config, enabled).await;
        Ok(supervisor)
    }

    /// Spawn `servers` and wait for each to register, recording the ones that
    /// did not into `self.unregistered`. Per-server failures are warnings: the
    /// worker stays usable with whatever registered.
    async fn start_servers(
        &mut self,
        config: &ToolServerStartConfig,
        servers: &[&ToolServerConfig],
        readiness_timeout: Duration,
    ) {
        let registry = match ensure_registry_bucket(&config.client, config.replicas.unwrap_or(1))
            .await
        {
            Ok(registry) => registry,
            Err(error) => {
                for server in servers {
                    warn_server_failure(&server.name, format!("prepare tool registry: {error:#}"));
                    self.unregistered.push(SupervisedServer::new(
                        server.package.as_deref(),
                        &server.name,
                    ));
                }
                return;
            }
        };

        let processes = Arc::clone(&self.processes);
        let running = running_server_names(&processes).await;
        let plan = prepare_registration_watches(&registry, config, servers, &running).await;
        let watches = plan.watches;
        self.unregistered.extend(plan.unwatchable);
        spawn_enabled_tool_servers(self, config, servers, &running).await;

        let deadline = Instant::now() + readiness_timeout;
        let instance_id = config.instance_id.clone();
        let readiness = watches.into_iter().map(|(server, key, mut watch)| {
            let processes = Arc::clone(&processes);
            let instance_id = instance_id.clone();
            let identity = SupervisedServer::new(server.package.as_deref(), &server.name);
            async move {
                let wait = RegistrationWait {
                    processes: &processes,
                    identity: identity.clone(),
                    instance_id: &instance_id,
                    key: &key,
                    deadline,
                    timeout: readiness_timeout,
                };
                match wait_for_registration(&mut watch, wait).await {
                    Ok(()) => None,
                    Err(error) => {
                        warn_server_failure(&identity.label(), format!("{error:#}"));
                        Some(identity)
                    }
                }
            }
        });
        self.unregistered.extend(
            futures_util::future::join_all(readiness)
                .await
                .into_iter()
                .flatten(),
        );
        if !self.unregistered.is_empty() {
            log_registry_contents(&registry, &config.instance_id).await;
        }
    }

    /// Labels of enabled servers still missing a registration.
    pub fn unregistered_servers(&self) -> Vec<String> {
        self.unregistered
            .iter()
            .map(SupervisedServer::label)
            .collect()
    }

    /// Respawn only the servers that have not registered yet and wait again.
    /// Returns whether any are still missing afterwards.
    pub async fn retry_unregistered(
        &mut self,
        config: &ToolServerStartConfig,
        servers: &[ToolServerConfig],
        readiness_timeout: Duration,
    ) -> bool {
        let pending = std::mem::take(&mut self.unregistered);
        let retry: Vec<_> = servers
            .iter()
            .filter(|server| {
                server.enabled
                    && pending.contains(&SupervisedServer::new(
                        server.package.as_deref(),
                        &server.name,
                    ))
            })
            .collect();
        if retry.is_empty() {
            return false;
        }
        self.start_servers(config, &retry, readiness_timeout).await;
        !self.unregistered.is_empty()
    }

    pub async fn server_pids(&self) -> HashMap<u32, String> {
        self.processes
            .lock()
            .await
            .iter()
            .map(|(pid, server)| (*pid, server.config.clone()))
            .collect()
    }

    /// Kill every supervised child, wait for it to actually exit, and remove
    /// its tool registration; then shut down every co-located hook
    /// supervisor. Unlike letting this value simply drop, this is what
    /// actually deregisters: each monitor task's own exit path (which calls
    /// `remove_registrations_for_config`) only runs if the task is allowed to
    /// observe its child's exit, and `HookServerSupervisor` has no `Drop` at
    /// all — only its explicit `shutdown` removes registrations and
    /// expectations.
    pub async fn shutdown(mut self) {
        self.shutdown_signal.cancel();
        for task in std::mem::take(&mut self.tasks) {
            let _ = task.await;
        }
        for mut hooks in std::mem::take(&mut self.hook_supervisors) {
            hooks.shutdown().await;
        }
    }
}

/// Names of servers with a live child process, so retries neither double-spawn
/// a server that is merely slow to register nor clear a registration it is
/// about to publish.
async fn running_server_names(processes: &SupervisedProcesses) -> HashSet<SupervisedServer> {
    processes.lock().await.values().cloned().collect()
}

async fn prepare_registration_watches<'a>(
    registry: &kv::Store,
    config: &ToolServerStartConfig,
    servers: &[&'a ToolServerConfig],
    running: &HashSet<SupervisedServer>,
) -> WatchPlan<'a> {
    let mut plan = WatchPlan::default();
    for server in servers {
        let expected_token =
            server_identity_token(server.package.as_deref(), &server.name, "<server>");
        let key = registration_key(&config.instance_id, &expected_token);
        // Only clear stale registrations for servers we are about to respawn.
        // A still-running server may publish its registration at any moment;
        // deleting it here would lose a result the watch below cannot replay.
        if !running.contains(&SupervisedServer::new(
            server.package.as_deref(),
            &server.name,
        )) {
            remove_registrations_for_config(
                &config.client,
                &config.instance_id,
                server.package.as_deref(),
                &server.name,
                None,
            )
            .await;
        }
        match registry.watch_with_history(">").await {
            Ok(watch) => plan.watches.push((*server, key, watch)),
            Err(error) => {
                warn_server_failure(
                    &server.name,
                    format!("watch tool registration '{key}': {error:#}"),
                );
                // Still record it as unregistered. The child is spawned either
                // way, and only servers in `unregistered` are ever retried, so
                // dropping it here would strand a server that merely lost its
                // watch.
                plan.unwatchable.push(SupervisedServer::new(
                    server.package.as_deref(),
                    &server.name,
                ));
            }
        }
    }
    plan
}

/// Watches to await, plus the servers that could not get one.
#[derive(Default)]
struct WatchPlan<'a> {
    watches: Vec<(&'a ToolServerConfig, String, kv::Watch)>,
    unwatchable: Vec<SupervisedServer>,
}

async fn spawn_enabled_tool_servers(
    supervisor: &mut ToolServerSupervisor,
    config: &ToolServerStartConfig,
    servers: &[&ToolServerConfig],
    running: &HashSet<SupervisedServer>,
) {
    let processes = Arc::clone(&supervisor.processes);
    let in_flight = NatsInFlightCalls::for_instance(&config.instance_id);
    let shutdown_signal = supervisor.shutdown_signal.clone();
    for server in servers {
        // A server whose child is still alive is slow, not dead — give the
        // existing process more time rather than stacking a second copy.
        if running.contains(&SupervisedServer::new(
            server.package.as_deref(),
            &server.name,
        )) {
            continue;
        }
        match spawn_tool_server(config, server).await {
            Ok(child) => {
                let Some(pid) = child.id() else {
                    warn_server_failure(&server.name, "spawned child has no process ID");
                    continue;
                };
                processes.lock().await.insert(
                    pid,
                    SupervisedServer::new(server.package.as_deref(), &server.name),
                );
                supervisor.tasks.push(spawn_child_monitor(ToolMonitor {
                    child,
                    pid,
                    server: server.name.clone(),
                    package: server.package.clone(),
                    config: server.name.clone(),
                    instance_id: config.instance_id.clone(),
                    client: config.client.clone(),
                    processes: Arc::clone(&processes),
                    in_flight: in_flight.clone(),
                    shutdown_signal: shutdown_signal.clone(),
                }));
            }
            Err(error) => warn_server_failure(&server.name, format!("{error:#}")),
        }
    }
}

async fn start_co_located_hooks(
    supervisor: &mut ToolServerSupervisor,
    config: &ToolServerStartConfig,
    servers: Vec<&ToolServerConfig>,
) {
    for server in servers {
        let Some(hooks) = &server.hooks else {
            continue;
        };
        let mut hooks = hooks.clone();
        let package_dir = tool_server_package_dir(server);
        for hook in &mut hooks.entries {
            if hook.package_dir.is_none() {
                hook.package_dir = Some(package_dir.clone());
            }
        }
        let scope = format!("tool-{}", server.name);
        match HookServerSupervisor::start_local(config.hook_start_config(), &hooks, &scope).await {
            Ok(hooks) => supervisor.hook_supervisors.push(hooks),
            Err(error) => warn_server_failure(
                &server.name,
                format!("start co-located hook servers: {error:#}"),
            ),
        }
    }
}

impl Drop for ToolServerSupervisor {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn resolve_tool_binary(server: &ToolServerConfig) -> Result<PathBuf> {
    let current = std::env::current_exe().context("resolve current worker executable")?;
    let directory = current
        .parent()
        .context("current worker executable has no parent directory")?;
    let directory = if directory.file_name().is_some_and(|name| name == "deps") {
        directory
            .parent()
            .context("test executable deps directory has no parent")?
    } else {
        directory
    };
    let next_to_worker = directory.join(&server.command);
    #[cfg(windows)]
    let next_to_worker = if next_to_worker.extension().is_none() {
        next_to_worker.with_extension("exe")
    } else {
        next_to_worker
    };
    if next_to_worker.is_file() {
        return Ok(next_to_worker);
    }

    which::which(&server.command).with_context(|| {
        format!(
            "tool-server command '{}' not found next to worker at {} or on PATH",
            server.command,
            next_to_worker.display()
        )
    })
}

fn tool_server_package_dir(server: &ToolServerConfig) -> PathBuf {
    server
        .package
        .as_deref()
        .map(harnx_core::config_paths::package_dir)
        .unwrap_or_else(harnx_core::config_paths::config_dir)
}

fn child_output_sink(config: &ToolServerStartConfig) -> Stdio {
    if config.inherit_child_output {
        Stdio::inherit()
    } else {
        harnx_core::logging::child_output_sink()
    }
}

/// Mirror this config's TLS/mTLS settings into the child's environment, using
/// the exact same variable names `NatsEndpoint::from_env` reads. A spawned
/// server that can't see these connects plaintext to a TLS-only broker and
/// never reaches it.
fn apply_tls_env(command: &mut Command, config: &ToolServerStartConfig) {
    if let Some(tls) = config.tls {
        command.env(HARNX_NATS_TLS_ENV, if tls { "true" } else { "false" });
    }
    if let Some(cert) = &config.tls_cert {
        command.env(HARNX_NATS_TLS_CERT_ENV, cert);
    }
    if let Some(key) = &config.tls_key {
        command.env(HARNX_NATS_TLS_KEY_ENV, key);
    }
    if let Some(ca) = &config.tls_ca {
        command.env(HARNX_NATS_TLS_CA_ENV, ca);
    }
}

async fn spawn_tool_server(
    config: &ToolServerStartConfig,
    server: &ToolServerConfig,
) -> Result<Child> {
    let binary = resolve_tool_binary(server)?;
    let mut command = Command::new(&binary);
    command
        .args(&server.args)
        .env(HARNX_PACKAGE_DIR_ENV, tool_server_package_dir(server))
        .envs(&server.env)
        .env(
            HARNX_SERVER_PACKAGE,
            server.package.as_deref().unwrap_or_default(),
        )
        .env(HARNX_SERVER_CONFIG, &server.name)
        .env(HARNX_SERVER_SCOPE, config.instance_id.as_str())
        .env(HARNX_NATS_URL_ENV, &config.nats_url)
        .env(HARNX_NATS_TOKEN_ENV, &config.token)
        .env(
            HARNX_NATS_REPLICAS_ENV,
            config.replicas.unwrap_or(1).to_string(),
        );
    apply_tls_env(&mut command, config);
    command
        .stdin(Stdio::null())
        // Send output to the worker log instead of discarding it, so a tool
        // server that dies or complains on startup leaves a trace there rather
        // than silently timing out at registration.
        .stdout(child_output_sink(config))
        .stderr(child_output_sink(config))
        .kill_on_drop(true);
    config
        .process_manager
        .spawn(command)
        .await
        .with_context(|| {
            format!(
                "spawn tool server '{}' from {}",
                server.name,
                binary.display()
            )
        })
}

/// Report a tool-server problem. Deliberately not phrased as "failed to start":
/// a server that is merely slow is reported through here too.
fn warn_server_failure(server: &str, detail: impl std::fmt::Display) {
    emit_warning(format!("tool server '{server}': {detail}"));
}

fn emit_warning(message: String) {
    // `emit_agent_event` reports success even with no sink installed: it buffers
    // into a capped queue for replay once one appears. That works for the
    // front-end, but the worker has no sink while it starts up, so these
    // warnings would sit in the queue until some later session replayed them —
    // or be dropped entirely if the worker never gets one. Always write to
    // stderr too; the supervisor redirects it to the worker log, which is the
    // only channel that survives worker startup.
    emit_agent_event(AgentEvent::Notice(NoticeEvent::Warning(message.clone())));
    eprintln!("Warning: {message}");
    log::warn!("{message}");
}

struct ToolMonitor {
    child: Child,
    pid: u32,
    server: String,
    package: Option<String>,
    config: String,
    instance_id: ServerScope,
    client: async_nats::Client,
    processes: SupervisedProcesses,
    in_flight: NatsInFlightCalls,
    shutdown_signal: CancellationToken,
}

/// Outcome of waiting for a child: either it exited on its own (a crash, from
/// this task's point of view — nothing here asked for that) or `shutdown`
/// killed it deliberately.
enum ChildExit {
    Crashed(std::io::Result<std::process::ExitStatus>),
    Shutdown,
}

/// Wait for the child to exit on its own, or, once `shutdown_signal` fires,
/// kill it and wait for that exit. Killing here rather than only relying on
/// `kill_on_drop` is what lets this task reach its own cleanup below
/// (registration removal) instead of a `Drop`/abort racing past it.
///
/// `shutdown_signal.cancelled()`, not a `Notify::notified()`: cancellation is
/// durable, so a monitor that reaches this `select!` only after `shutdown`
/// already called `cancel()` still observes it immediately, rather than
/// waiting on a wakeup that already fired for whichever waiters existed at
/// the time.
async fn wait_for_child_or_shutdown(
    child: &mut Child,
    shutdown_signal: &CancellationToken,
) -> ChildExit {
    tokio::select! {
        status = wait_for_child(child) => ChildExit::Crashed(status),
        _ = shutdown_signal.cancelled() => {
            let _ = child.start_kill();
            let _ = wait_for_child(child).await;
            ChildExit::Shutdown
        }
    }
}

fn spawn_child_monitor(monitor: ToolMonitor) -> JoinHandle<()> {
    let ToolMonitor {
        mut child,
        pid,
        server,
        package,
        config,
        instance_id,
        client,
        processes,
        in_flight,
        shutdown_signal,
    } = monitor;
    tokio::spawn(async move {
        let exit = wait_for_child_or_shutdown(&mut child, &shutdown_signal).await;
        processes.lock().await.remove(&pid);
        let message = match exit {
            ChildExit::Shutdown => format!("tool server '{server}' stopped"),
            ChildExit::Crashed(status) => {
                let exit = match status {
                    Ok(status) => status
                        .code()
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| status.to_string()),
                    Err(error) => format!("wait error: {error}"),
                };
                format!("tool server '{server}' crashed, exit {exit}")
            }
        };
        emit_warning(message.clone());
        remove_registrations_for_config(
            &client,
            &instance_id,
            package.as_deref(),
            &config,
            Some((&in_flight, &message)),
        )
        .await;
    })
}

#[cfg(target_os = "linux")]
async fn wait_for_child(child: &mut Child) -> std::io::Result<std::process::ExitStatus> {
    // Tokio's Linux process driver uses pidfd/PidfdReaper when available.
    child.wait().await
}

#[cfg(not(target_os = "linux"))]
async fn wait_for_child(child: &mut Child) -> std::io::Result<std::process::ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_server(package: Option<&str>) -> ToolServerConfig {
        ToolServerConfig {
            name: "test".to_string(),
            command: "test".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            enabled: true,
            description: None,
            package: package.map(str::to_string),
            hooks: None,
        }
    }

    #[test]
    fn package_owned_tool_server_uses_package_dir() {
        let server = tool_server(Some("coding"));

        assert_eq!(
            tool_server_package_dir(&server),
            harnx_core::config_paths::packages_dir().join("coding")
        );
    }

    #[test]
    fn user_tool_server_uses_config_dir() {
        let server = tool_server(None);

        assert_eq!(
            tool_server_package_dir(&server),
            harnx_core::config_paths::config_dir()
        );
    }

    /// Regression: a monitor task that reaches `wait_for_child_or_shutdown`
    /// *after* `shutdown` already signaled must still see the shutdown, not
    /// hang waiting for the child to exit on its own (which, for a live tool
    /// server, is never). `Notify::notify_waiters()` would lose this exact
    /// ordering — it only wakes waiters already parked on `notified()` at the
    /// moment it's called — which is why the signal is a `CancellationToken`
    /// instead: cancellation is durable state, observed correctly no matter
    /// when a caller checks it.
    ///
    /// Unix-only: uses `sleep` as a real, portable, definitely-alive child;
    /// Windows CI has no equivalent without extra ceremony this doesn't need.
    #[cfg(unix)]
    #[tokio::test]
    async fn wait_for_child_or_shutdown_observes_a_cancellation_that_already_fired() {
        harnx_core::require_nextest();
        let shutdown_signal = CancellationToken::new();
        // Cancel before anything has ever polled `cancelled()` on this
        // token — the exact ordering the old `Notify` lost.
        shutdown_signal.cancel();

        let mut child = tokio::process::Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sleep");

        let exit = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_child_or_shutdown(&mut child, &shutdown_signal),
        )
        .await
        .expect(
            "wait_for_child_or_shutdown must observe an already-fired \
             cancellation, not hang waiting for the child to exit on its own",
        );

        assert!(matches!(exit, ChildExit::Shutdown));
    }
}
