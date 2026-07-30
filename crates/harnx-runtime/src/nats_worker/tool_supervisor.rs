use crate::config::{ToolServerConfig, HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV};
use crate::nats_tool_provider::NatsInFlightCalls;
use anyhow::{bail, Context, Result};
use async_nats::jetstream::{self, kv, stream};
use futures_util::StreamExt;
use harnx_core::event::{AgentEvent, NoticeEvent};
use harnx_core::instance::{InstanceId, HARNX_INSTANCE_ID};
use harnx_core::sink::emit_agent_event;
use harnx_toolset::Registration;
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
}

/// Owns local tool-server children for one worker process.
pub struct ToolServerSupervisor {
    processes: Arc<Mutex<HashMap<u32, String>>>,
    tasks: Vec<JoinHandle<()>>,
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

        let mut watches = Vec::new();
        for server in &enabled {
            let key = registration_key(&config.instance_id, &server.name);
            let _ = registry.delete(&key).await;
            match registry.watch_with_history(&key).await {
                Ok(watch) => watches.push((*server, key, watch)),
                Err(error) => warn_server_failure(
                    &server.name,
                    format!("watch tool registration '{key}': {error:#}"),
                ),
            }
        }

        let in_flight = NatsInFlightCalls::for_instance(&config.instance_id);
        for server in enabled {
            match spawn_tool_server(&config, server) {
                Ok(child) => {
                    let Some(pid) = child.id() else {
                        warn_server_failure(&server.name, "spawned child has no process ID");
                        continue;
                    };
                    processes.lock().await.insert(pid, server.name.clone());
                    supervisor.tasks.push(spawn_child_monitor(
                        child,
                        pid,
                        server.name.clone(),
                        config.instance_id.clone(),
                        config.client.clone(),
                        Arc::clone(&processes),
                        in_flight.clone(),
                    ));
                }
                Err(error) => warn_server_failure(&server.name, format!("{error:#}")),
            }
        }

        let deadline = Instant::now() + readiness_timeout;
        let readiness = watches.into_iter().map(|(server, key, mut watch)| {
            let processes = Arc::clone(&processes);
            async move {
                if let Err(error) = wait_for_registration(
                    &mut watch,
                    &processes,
                    &server.name,
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
        Ok(supervisor)
    }

    pub async fn server_pids(&self) -> HashMap<u32, String> {
        self.processes.lock().await.clone()
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

fn spawn_tool_server(config: &ToolServerStartConfig, server: &ToolServerConfig) -> Result<Child> {
    let binary = resolve_tool_binary(server)?;
    let mut command = Command::new(&binary);
    command
        .args(&server.args)
        .envs(&server.env)
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

fn spawn_child_monitor(
    mut child: Child,
    pid: u32,
    server: String,
    instance_id: InstanceId,
    client: async_nats::Client,
    processes: Arc<Mutex<HashMap<u32, String>>>,
    in_flight: NatsInFlightCalls,
) -> JoinHandle<()> {
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
        in_flight.fail_server_unavailable(&server, message).await;
        remove_registration(&client, &instance_id, &server).await;
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
    server: &str,
    key: &str,
    deadline: Instant,
    timeout: Duration,
) -> Result<()> {
    loop {
        if !processes.lock().await.values().any(|name| name == server) {
            bail!("tool server '{server}' exited before registering at '{key}'");
        }
        let now = Instant::now();
        if now >= deadline {
            bail!(
                "tool server '{server}' did not register at '{key}' within {}s",
                timeout.as_secs_f64()
            );
        }
        let remaining = deadline.saturating_duration_since(now);
        let poll = remaining.min(Duration::from_millis(100));
        match tokio::time::timeout(poll, watch.next()).await {
            Ok(Some(Ok(entry))) if entry.operation == kv::Operation::Put => {
                let registration: Registration = serde_json::from_slice(&entry.value)
                    .with_context(|| format!("decode tool registration '{key}'"))?;
                if registration.server == server {
                    return Ok(());
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(error))) => {
                return Err(error).with_context(|| format!("watch tool registration '{key}'"));
            }
            Ok(None) => bail!("tool registration watch closed for '{key}'"),
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

async fn remove_registration(client: &async_nats::Client, instance_id: &InstanceId, server: &str) {
    let jetstream = jetstream::new(client.clone());
    let Ok(store) = jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await else {
        return;
    };
    let _ = store.delete(registration_key(instance_id, server)).await;
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
