use super::hook_supervisor::{HookServerStartConfig, HookServerSupervisor};
use super::tool_registry::{
    ensure_registry_bucket, log_registry_contents, remove_registrations_for_config,
    wait_for_registration, RegistrationWait, SupervisedProcesses, SupervisedServer,
};
use crate::config::{ToolServerConfig, HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV};
use crate::nats_tool_provider::NatsInFlightCalls;
use anyhow::{Context, Result};
use async_nats::jetstream::kv;
use harnx_core::event::{AgentEvent, NoticeEvent};
use harnx_core::instance::{InstanceId, HARNX_INSTANCE_ID};
use harnx_core::sink::emit_agent_event;
use harnx_hooks::executor::HARNX_PACKAGE_DIR_ENV;
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

const TOOL_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

/// Connection and identity shared by all tool servers spawned for one worker.
#[derive(Clone)]
pub struct ToolServerStartConfig {
    client: async_nats::Client,
    instance_id: InstanceId,
    nats_url: String,
    token: String,
}

impl ToolServerStartConfig {
    pub fn new(
        client: async_nats::Client,
        instance_id: InstanceId,
        nats_url: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        Self {
            client,
            instance_id,
            nats_url: nats_url.into(),
            token: token.into(),
        }
    }

    fn hook_start_config(&self) -> HookServerStartConfig {
        HookServerStartConfig::new(
            self.client.clone(),
            self.instance_id.clone(),
            self.nats_url.clone(),
            self.token.clone(),
        )
    }
}

/// Owns local tool-server children for one worker process.
pub struct ToolServerSupervisor {
    processes: SupervisedProcesses,
    tasks: Vec<JoinHandle<()>>,
    hook_supervisors: Vec<HookServerSupervisor>,
    /// Enabled servers that have not registered yet. Retried in the background
    /// by the worker, so this shrinks as late registrations land.
    unregistered: Vec<SupervisedServer>,
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
            processes: Arc::clone(&processes),
            tasks: Vec::new(),
            hook_supervisors: Vec::new(),
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
        let registry = match ensure_registry_bucket(&config.client).await {
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
    for server in servers {
        // A server whose child is still alive is slow, not dead — give the
        // existing process more time rather than stacking a second copy.
        if running.contains(&SupervisedServer::new(
            server.package.as_deref(),
            &server.name,
        )) {
            continue;
        }
        match spawn_tool_server(config, server) {
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

fn spawn_tool_server(config: &ToolServerStartConfig, server: &ToolServerConfig) -> Result<Child> {
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
        .env(HARNX_INSTANCE_ID, config.instance_id.as_str())
        .env(HARNX_NATS_URL_ENV, &config.nats_url)
        .env(HARNX_NATS_TOKEN_ENV, &config.token)
        .stdin(Stdio::null())
        // Send output to the worker log instead of discarding it, so a tool
        // server that dies or complains on startup leaves a trace there rather
        // than silently timing out at registration.
        .stdout(crate::local_orchestrator::worker_output_sink())
        .stderr(crate::local_orchestrator::worker_output_sink())
        .kill_on_drop(true);
    configure_tool_process(&mut command);
    command.spawn().with_context(|| {
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
    instance_id: InstanceId,
    client: async_nats::Client,
    processes: SupervisedProcesses,
    in_flight: NatsInFlightCalls,
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
    } = monitor;
    tokio::spawn(async move {
        let status = wait_for_child(&mut child).await;
        processes.lock().await.remove(&pid);
        let exit = match status {
            Ok(status) => status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| status.to_string()),
            Err(error) => format!("wait error: {error}"),
        };
        let message = format!("tool server '{server}' crashed, exit {exit}");
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

#[cfg(unix)]
fn configure_tool_process(command: &mut Command) {
    #[cfg(target_os = "linux")]
    let parent_pid = std::process::id() as libc::pid_t;
    // SAFETY: pre_exec invokes only async-signal-safe libc calls.
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    libc::raise(libc::SIGTERM);
                }
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_tool_process(_command: &mut Command) {}

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
}
