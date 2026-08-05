use super::hook_supervisor::{HookServerStartConfig, HookServerSupervisor};
use crate::config::{ToolServerConfig, HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV};
use crate::nats_tool_provider::NatsInFlightCalls;
use anyhow::{bail, Context, Result};
use async_nats::jetstream::{self, kv, stream};
use futures_util::{StreamExt, TryStreamExt};
use harnx_core::event::{AgentEvent, NoticeEvent};
use harnx_core::instance::{InstanceId, HARNX_INSTANCE_ID};
use harnx_core::sink::emit_agent_event;
use harnx_hooks::executor::HARNX_PACKAGE_DIR_ENV;
use harnx_toolset::{
    server_identity_token, Registration, HARNX_SERVER_CONFIG, HARNX_SERVER_PACKAGE,
};
use harnx_toolset_server::{registration_key, TOOL_REGISTRY_BUCKET};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

const TOOL_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

/// Connection and identity shared by all tool servers spawned for one worker.
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
    processes: Arc<Mutex<HashMap<u32, String>>>,
    tasks: Vec<JoinHandle<()>>,
    hook_supervisors: Vec<HookServerSupervisor>,
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
        };
        let enabled: Vec<_> = servers.iter().filter(|server| server.enabled).collect();
        if enabled.is_empty() {
            return Ok(supervisor);
        }

        let registry = match ensure_registry_bucket(&config.client).await {
            Ok(registry) => registry,
            Err(error) => {
                for server in enabled {
                    warn_server_failure(&server.name, format!("prepare tool registry: {error:#}"));
                }
                return Ok(supervisor);
            }
        };

        let watches = prepare_registration_watches(&registry, &config, &enabled).await;

        spawn_enabled_tool_servers(&mut supervisor, &config, &enabled, &processes).await;

        let deadline = Instant::now() + readiness_timeout;
        let instance_id = config.instance_id.clone();
        let readiness = watches.into_iter().map(|(server, key, mut watch)| {
            let processes = Arc::clone(&processes);
            let instance_id = instance_id.clone();
            async move {
                if let Err(error) = wait_for_registration(
                    &mut watch,
                    &processes,
                    server,
                    &instance_id,
                    &key,
                    deadline,
                    readiness_timeout,
                )
                .await
                {
                    warn_server_failure(&server.name, format!("{error:#}"));
                }
            }
        });
        futures_util::future::join_all(readiness).await;

        start_co_located_hooks(&mut supervisor, &config, enabled).await;
        Ok(supervisor)
    }

    pub async fn server_pids(&self) -> HashMap<u32, String> {
        self.processes.lock().await.clone()
    }
}

async fn prepare_registration_watches<'a>(
    registry: &kv::Store,
    config: &ToolServerStartConfig,
    servers: &[&'a ToolServerConfig],
) -> Vec<(&'a ToolServerConfig, String, kv::Watch)> {
    let mut watches = Vec::new();
    for server in servers {
        let expected_token =
            server_identity_token(server.package.as_deref(), &server.name, "<server>");
        let key = registration_key(&config.instance_id, &expected_token);
        remove_registrations_for_config(
            &config.client,
            &config.instance_id,
            server.package.as_deref(),
            &server.name,
            None,
        )
        .await;
        match registry.watch_with_history(">").await {
            Ok(watch) => watches.push((*server, key, watch)),
            Err(error) => warn_server_failure(
                &server.name,
                format!("watch tool registration '{key}': {error:#}"),
            ),
        }
    }
    watches
}

async fn spawn_enabled_tool_servers(
    supervisor: &mut ToolServerSupervisor,
    config: &ToolServerStartConfig,
    servers: &[&ToolServerConfig],
    processes: &Arc<Mutex<HashMap<u32, String>>>,
) {
    let in_flight = NatsInFlightCalls::for_instance(&config.instance_id);
    for server in servers {
        match spawn_tool_server(config, server) {
            Ok(child) => {
                let Some(pid) = child.id() else {
                    warn_server_failure(&server.name, "spawned child has no process ID");
                    continue;
                };
                processes.lock().await.insert(pid, server.name.clone());
                supervisor.tasks.push(spawn_child_monitor(ToolMonitor {
                    child,
                    pid,
                    server: server.name.clone(),
                    package: server.package.clone(),
                    config: server.name.clone(),
                    instance_id: config.instance_id.clone(),
                    client: config.client.clone(),
                    processes: Arc::clone(processes),
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
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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

fn warn_server_failure(server: &str, detail: impl std::fmt::Display) {
    emit_warning(format!("tool server '{server}' failed to start: {detail}"));
}

fn emit_warning(message: String) {
    if !emit_agent_event(AgentEvent::Notice(NoticeEvent::Warning(message.clone()))) {
        eprintln!("Warning: {message}");
    }
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
    processes: Arc<Mutex<HashMap<u32, String>>>,
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

async fn wait_for_registration(
    watch: &mut kv::Watch,
    processes: &Arc<Mutex<HashMap<u32, String>>>,
    server: &ToolServerConfig,
    instance_id: &InstanceId,
    key: &str,
    deadline: Instant,
    timeout: Duration,
) -> Result<()> {
    loop {
        if !processes
            .lock()
            .await
            .values()
            .any(|name| name == &server.name)
        {
            bail!(
                "tool server '{}' exited before registering at '{key}'",
                server.name
            );
        }
        let now = Instant::now();
        if now >= deadline {
            bail!(
                "tool server '{}' did not register at '{key}' within {}s",
                server.name,
                timeout.as_secs_f64()
            );
        }
        let remaining = deadline.saturating_duration_since(now);
        let poll = remaining.min(Duration::from_millis(100));
        match tokio::time::timeout(poll, watch.next()).await {
            Ok(Some(Ok(entry))) if entry.operation == kv::Operation::Put => {
                if !entry.key.starts_with(key.trim_end_matches("<server>")) {
                    continue;
                }
                let registration: Registration = match serde_json::from_slice(&entry.value) {
                    Ok(registration) => registration,
                    Err(error) => {
                        log::warn!(
                            "ignoring invalid tool registration '{}': {error}",
                            entry.key
                        );
                        continue;
                    }
                };
                if registration.package != server.package || registration.config != server.name {
                    log::warn!(
                        "ignoring tool registration '{}' identity mismatch: expected package {:?}, config '{}'; got package {:?}, config '{}'",
                        entry.key,
                        server.package,
                        server.name,
                        registration.package,
                        registration.config
                    );
                    continue;
                }
                let identity_token = server_identity_token(
                    registration.package.as_deref(),
                    &registration.config,
                    &registration.server,
                );
                let expected_key = registration_key(instance_id, &identity_token);
                if entry.key != expected_key {
                    log::warn!(
                        "ignoring tool registration '{}' because its identity expects key '{}'",
                        entry.key,
                        expected_key
                    );
                    continue;
                }
                return Ok(());
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(error))) => {
                log::warn!("ignoring tool registration watch error for '{key}': {error}");
            }
            Ok(None) => {
                log::warn!("tool registration watch closed for '{key}'; waiting for timeout");
                tokio::time::sleep(poll).await;
            }
            Err(_) => continue,
        }
    }
}

async fn ensure_registry_bucket(client: &async_nats::Client) -> Result<kv::Store> {
    let jetstream = jetstream::new(client.clone());
    match jetstream
        .create_key_value(kv::Config {
            bucket: TOOL_REGISTRY_BUCKET.to_string(),
            history: 1,
            num_replicas: 1,
            storage: stream::StorageType::File,
            ..Default::default()
        })
        .await
    {
        Ok(store) => Ok(store),
        Err(_) => jetstream
            .get_key_value(TOOL_REGISTRY_BUCKET)
            .await
            .map_err(anyhow::Error::from)
            .context("open tool registry bucket"),
    }
}

async fn remove_registrations_for_config(
    client: &async_nats::Client,
    instance_id: &InstanceId,
    package: Option<&str>,
    config: &str,
    failure: Option<(&NatsInFlightCalls, &str)>,
) {
    let jetstream = jetstream::new(client.clone());
    let Ok(store) = jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await else {
        return;
    };
    let Ok(mut keys) = store.keys().await else {
        return;
    };
    let prefix = format!("{instance_id}.");
    while let Ok(Some(key)) = keys.try_next().await {
        if !key.starts_with(&prefix) {
            continue;
        }
        let Ok(Some(value)) = store.get(&key).await else {
            continue;
        };
        let Ok(registration) = serde_json::from_slice::<Registration>(&value) else {
            continue;
        };
        if registration.package.as_deref() != package || registration.config != config {
            continue;
        }
        let identity_token = server_identity_token(
            registration.package.as_deref(),
            &registration.config,
            &registration.server,
        );
        if key != registration_key(instance_id, &identity_token) {
            continue;
        }
        if let Some((in_flight, message)) = failure {
            in_flight
                .fail_server_unavailable(&identity_token, message.to_string())
                .await;
        }
        let _ = store.delete(key).await;
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
