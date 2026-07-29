use crate::config::{HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV};
use crate::nats_tool_provider::NatsInFlightCalls;
use anyhow::{bail, Context, Result};
use async_nats::jetstream::{self, kv, stream};
use futures_util::StreamExt;
use harnx_core::instance::{InstanceId, HARNX_INSTANCE_ID};
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

pub const HARNX_TIME_SERVER_BIN: &str = "HARNX_TIME_SERVER_BIN";
const TOOL_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy)]
struct BootstrapServer {
    server: &'static str,
    binary: &'static str,
    override_env: &'static str,
}

const LOCAL_BOOTSTRAP_SERVERS: &[BootstrapServer] = &[BootstrapServer {
    server: "time",
    binary: "harnx-time-server",
    override_env: HARNX_TIME_SERVER_BIN,
}];

/// Owns local tool-server children for one worker process.
pub struct ToolServerSupervisor {
    processes: Arc<Mutex<HashMap<u32, String>>>,
    tasks: Vec<JoinHandle<()>>,
}

impl ToolServerSupervisor {
    /// Spawn the built-in local pilot and wait for its registration.
    pub async fn start_local(
        client: async_nats::Client,
        instance_id: InstanceId,
        nats_url: &str,
        token: &str,
    ) -> Result<Self> {
        Self::start_local_with_timeout(client, instance_id, nats_url, token, TOOL_STARTUP_TIMEOUT)
            .await
    }

    /// Same startup path with an explicit readiness timeout for tests and callers.
    pub async fn start_local_with_timeout(
        client: async_nats::Client,
        instance_id: InstanceId,
        nats_url: &str,
        token: &str,
        readiness_timeout: Duration,
    ) -> Result<Self> {
        let registry = ensure_registry_bucket(&client).await?;
        let mut watches = Vec::new();
        for bootstrap in LOCAL_BOOTSTRAP_SERVERS {
            let key = registration_key(&instance_id, bootstrap.server);
            let _ = registry.delete(&key).await;
            let watch = registry
                .watch_with_history(&key)
                .await
                .with_context(|| format!("watch tool registration '{key}'"))?;
            watches.push((*bootstrap, key, watch));
        }

        let processes = Arc::new(Mutex::new(HashMap::new()));
        let in_flight = NatsInFlightCalls::for_instance(&instance_id);
        let mut supervisor = Self {
            processes: Arc::clone(&processes),
            tasks: Vec::new(),
        };
        for bootstrap in LOCAL_BOOTSTRAP_SERVERS {
            let binary = resolve_tool_binary(bootstrap)?;
            let child =
                spawn_tool_server(&binary, &instance_id, nats_url, token, bootstrap.server)?;
            let pid = child
                .id()
                .with_context(|| format!("{} child has no process ID", bootstrap.binary))?;
            processes
                .lock()
                .await
                .insert(pid, bootstrap.server.to_string());
            supervisor.tasks.push(spawn_child_monitor(
                child,
                pid,
                bootstrap.server.to_string(),
                instance_id.clone(),
                client.clone(),
                Arc::clone(&processes),
                in_flight.clone(),
            ));
        }

        let deadline = Instant::now() + readiness_timeout;
        for (bootstrap, key, mut watch) in watches {
            wait_for_registration(
                &mut watch,
                &processes,
                bootstrap.server,
                &key,
                deadline,
                readiness_timeout,
            )
            .await?;
        }
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

fn resolve_tool_binary(bootstrap: &BootstrapServer) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(bootstrap.override_env) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        bail!(
            "{} points to a missing tool-server binary: {}",
            bootstrap.override_env,
            path.display()
        );
    }
    let current = std::env::current_exe().context("resolve current worker executable")?;
    let directory = current
        .parent()
        .context("current worker executable has no parent directory")?;
    let mut path = directory.join(bootstrap.binary);
    if directory.file_name().is_some_and(|name| name == "deps") {
        path = directory
            .parent()
            .context("test executable deps directory has no parent")?
            .join(bootstrap.binary);
    }
    #[cfg(windows)]
    path.set_extension("exe");
    if !path.is_file() {
        bail!(
            "{} binary not found next to worker at {}; set {}",
            bootstrap.binary,
            path.display(),
            bootstrap.override_env
        );
    }
    Ok(path)
}

fn spawn_tool_server(
    binary: &PathBuf,
    instance_id: &InstanceId,
    nats_url: &str,
    token: &str,
    server: &str,
) -> Result<Child> {
    let mut command = Command::new(binary);
    command
        .env(HARNX_INSTANCE_ID, instance_id.as_str())
        .env(HARNX_NATS_URL_ENV, nats_url)
        .env(HARNX_NATS_TOKEN_ENV, token)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    configure_tool_process(&mut command);
    command
        .spawn()
        .with_context(|| format!("spawn tool server '{server}' from {}", binary.display()))
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
