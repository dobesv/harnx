use crate::config::{HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV};
use anyhow::{bail, Context, Result};
use async_nats::jetstream::{self, kv, stream};
use futures_util::StreamExt;
use harnx_core::hooks::{HookConfig, HooksConfig};
use harnx_core::instance::{InstanceId, HARNX_INSTANCE_ID};
use harnx_hooks::executor::HARNX_PACKAGE_DIR_ENV;
use harnx_hookset::{
    FailPolicy, HookRegistration, HookSpec, HOOK_EXPECTATIONS_BUCKET, HOOK_PROTOCOL_VERSION,
    HOOK_REGISTRY_BUCKET, HOOK_SCHEMA_VERSION,
};
use harnx_hookset_server::hook_registration_key;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

const HOOK_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const GENERIC_HOOK_BINARY: &str = "harnx-claude-compatible-hook-server";
const PROXY_AUTH_BINARY: &str = "harnx-proxy-auth";

/// Connection and identity shared by hook servers spawned for one worker.
#[derive(Clone)]
pub struct HookServerStartConfig {
    client: async_nats::Client,
    instance_id: InstanceId,
    nats_url: String,
    token: String,
}

impl HookServerStartConfig {
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

/// Owns one child process per configured hook and removes registrations on exit.
pub struct HookServerSupervisor {
    processes: Arc<Mutex<HashMap<u32, String>>>,
    tasks: Vec<JoinHandle<()>>,
    client: async_nats::Client,
    instance_id: InstanceId,
    registrations: Vec<String>,
}

impl HookServerSupervisor {
    pub async fn start_local(
        config: HookServerStartConfig,
        hooks: &HooksConfig,
        scope: &str,
    ) -> Result<Self> {
        Self::start_local_with_timeout(config, hooks, scope, HOOK_STARTUP_TIMEOUT).await
    }

    pub async fn start_local_with_timeout(
        config: HookServerStartConfig,
        hooks: &HooksConfig,
        scope: &str,
        readiness_timeout: Duration,
    ) -> Result<Self> {
        let processes = Arc::new(Mutex::new(HashMap::new()));
        let mut supervisor = Self {
            processes: Arc::clone(&processes),
            tasks: Vec::new(),
            client: config.client.clone(),
            instance_id: config.instance_id.clone(),
            registrations: Vec::new(),
        };
        let enabled: Vec<_> = hooks
            .entries
            .iter()
            .enumerate()
            .filter(|(_, hook)| hook.is_supported_type())
            .collect();
        if enabled.is_empty() {
            return Ok(supervisor);
        }

        let watches = prepare_hook_registrations(&config, &enabled, scope, &mut supervisor).await?;
        spawn_enabled_hooks(&config, enabled, scope, &mut supervisor).await;
        wait_for_hook_registrations(
            watches,
            &processes,
            Instant::now() + readiness_timeout,
            readiness_timeout,
        )
        .await;
        Ok(supervisor)
    }

    pub async fn server_pids(&self) -> HashMap<u32, String> {
        self.processes.lock().await.clone()
    }

    /// Stop all children and remove their registry entries before returning.
    pub async fn shutdown(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        for server in std::mem::take(&mut self.registrations) {
            remove_registration_and_expectation(&self.client, &self.instance_id, &server).await;
        }
    }
}

type RegistrationWatch = (String, String, kv::Watch);

async fn prepare_hook_registrations(
    config: &HookServerStartConfig,
    enabled: &[(usize, &HookConfig)],
    scope: &str,
    supervisor: &mut HookServerSupervisor,
) -> Result<Vec<RegistrationWatch>> {
    let registry = ensure_bucket(&config.client, HOOK_REGISTRY_BUCKET).await?;
    let expectations = ensure_bucket(&config.client, HOOK_EXPECTATIONS_BUCKET).await?;
    let mut watches = Vec::new();
    for (index, hook) in enabled {
        let name = hook_server_name(scope, *index, hook);
        let key = hook_registration_key(&config.instance_id, &name);
        let _ = registry.delete(&key).await;
        let watch = registry
            .watch_with_history(&key)
            .await
            .with_context(|| format!("watch hook registration '{key}'"))?;
        publish_expectation(&expectations, &key, &name, hook).await?;
        supervisor.registrations.push(name.clone());
        watches.push((name, key, watch));
    }
    Ok(watches)
}

async fn spawn_enabled_hooks(
    config: &HookServerStartConfig,
    enabled: Vec<(usize, &HookConfig)>,
    scope: &str,
    supervisor: &mut HookServerSupervisor,
) {
    for (index, hook) in enabled {
        let name = hook_server_name(scope, index, hook);
        let child = match spawn_hook_server(config, hook, &name) {
            Ok(child) => child,
            Err(error) => {
                log::warn!(
                    "hook server '{name}' failed to start; fail-closed expectation remains: {error:#}"
                );
                continue;
            }
        };
        let Some(pid) = child.id() else {
            log::warn!("hook server '{name}' has no process ID; fail-closed expectation remains");
            continue;
        };
        supervisor.processes.lock().await.insert(pid, name.clone());
        supervisor.tasks.push(spawn_child_monitor(
            child,
            pid,
            name,
            config.instance_id.clone(),
            config.client.clone(),
            Arc::clone(&supervisor.processes),
        ));
    }
}

async fn wait_for_hook_registrations(
    watches: Vec<RegistrationWatch>,
    processes: &Arc<Mutex<HashMap<u32, String>>>,
    deadline: Instant,
    readiness_timeout: Duration,
) {
    let readiness = watches.into_iter().map(|(name, key, mut watch)| {
        let processes = Arc::clone(processes);
        async move {
            if let Err(error) = wait_for_registration(
                &mut watch,
                &processes,
                &name,
                &key,
                deadline,
                readiness_timeout,
            )
            .await
            {
                log::warn!(
                    "hook server '{name}' unavailable; fail-closed expectation remains: {error:#}"
                );
            }
        }
    });
    futures_util::future::join_all(readiness).await;
}

impl Drop for HookServerSupervisor {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        let client = self.client.clone();
        let instance_id = self.instance_id.clone();
        let registrations = std::mem::take(&mut self.registrations);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                for server in registrations {
                    remove_registration_and_expectation(&client, &instance_id, &server).await;
                }
            });
        }
    }
}

fn hook_server_name(scope: &str, index: usize, hook: &HookConfig) -> String {
    if is_proxy_auth_command(&hook.command) {
        return "proxy-auth".to_string();
    }
    let scope = sanitize_name(scope);
    let event = sanitize_name(&hook.event);
    format!("{scope}-{event}-{index}")
}

fn sanitize_name(value: &str) -> String {
    let value: String = value
        .chars()
        .map(|ch| if is_server_name_char(ch) { ch } else { '-' })
        .collect();
    value.trim_matches('-').to_string()
}

fn is_server_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')
}

fn spawn_hook_server(
    config: &HookServerStartConfig,
    hook: &HookConfig,
    name: &str,
) -> Result<Child> {
    let package_dir = hook
        .package_dir
        .clone()
        .unwrap_or_else(harnx_core::config_paths::config_dir);
    let mut command = if let Some(args) = proxy_auth_args(&hook.command, &package_dir)? {
        let binary = resolve_binary(PROXY_AUTH_BINARY)?;
        let mut command = Command::new(&binary);
        command.args(args);
        command
    } else {
        let binary = resolve_binary(GENERIC_HOOK_BINARY)?;
        let mut command = Command::new(&binary);
        command
            .arg("--name")
            .arg(name)
            .arg("--event")
            .arg(&hook.event)
            .arg("--priority")
            .arg("0")
            .arg("--fail-policy")
            .arg(FailPolicy::Closed.as_str())
            .arg("--type")
            .arg(&hook.hook_type)
            .arg("--command")
            .arg(&hook.command)
            .arg("--package-dir")
            .arg(&package_dir);
        if let Some(matcher) = &hook.matcher {
            command.arg("--matcher").arg(matcher);
        }
        if let Some(timeout) = hook.timeout {
            command.arg("--timeout").arg(timeout.to_string());
        }
        command
    };

    command
        .env(HARNX_PACKAGE_DIR_ENV, package_dir)
        .env(HARNX_INSTANCE_ID, config.instance_id.as_str())
        .env(HARNX_NATS_URL_ENV, &config.nats_url)
        .env(HARNX_NATS_TOKEN_ENV, &config.token)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    configure_hook_process(&mut command);
    command
        .spawn()
        .with_context(|| format!("spawn hook server '{name}'"))
}

/// A command whose executable basename is `harnx-proxy-auth` is already a
/// native hook server. Its remaining words are forwarded as normal CLI flags.
fn proxy_auth_args(command: &str, package_dir: &Path) -> Result<Option<Vec<String>>> {
    let words = shell_words::split(command).context("parse hook command")?;
    if !is_proxy_auth_words(&words) {
        return Ok(None);
    }
    let package_dir = package_dir.to_string_lossy();
    let args = words
        .into_iter()
        .skip(1)
        .map(|word| {
            word.replace("${HARNX_PACKAGE_DIR}", &package_dir)
                .replace("$HARNX_PACKAGE_DIR", &package_dir)
        })
        .collect();
    Ok(Some(args))
}

fn is_proxy_auth_command(command: &str) -> bool {
    shell_words::split(command).is_ok_and(|words| is_proxy_auth_words(&words))
}

fn is_proxy_auth_words(words: &[String]) -> bool {
    words
        .first()
        .and_then(|program| Path::new(program).file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == PROXY_AUTH_BINARY || name == "harnx-proxy-auth.exe")
}

fn resolve_binary(binary: &str) -> Result<PathBuf> {
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
    let next_to_worker = directory.join(binary);
    #[cfg(windows)]
    let next_to_worker = next_to_worker.with_extension("exe");
    if next_to_worker.is_file() {
        return Ok(next_to_worker);
    }
    which::which(binary).with_context(|| {
        format!(
            "hook-server command '{binary}' not found next to worker at {} or on PATH",
            next_to_worker.display()
        )
    })
}

fn spawn_child_monitor(
    mut child: Child,
    pid: u32,
    server: String,
    instance_id: InstanceId,
    client: async_nats::Client,
    processes: Arc<Mutex<HashMap<u32, String>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let status = child.wait().await;
        processes.lock().await.remove(&pid);
        remove_registration(&client, &instance_id, &server).await;
        // Keep the supervisor's expectation after an unexpected exit. Discovery
        // will route to the absent server and apply its fail-closed policy.
        match status {
            Ok(status) if status.success() => log::debug!("hook server '{server}' exited"),
            Ok(status) => log::warn!("hook server '{server}' exited with {status}"),
            Err(error) => log::warn!("hook server '{server}' wait failed: {error}"),
        }
    })
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
            bail!("hook server '{server}' exited before registering at '{key}'");
        }
        let now = Instant::now();
        if now >= deadline {
            bail!(
                "hook server '{server}' did not register at '{key}' within {}s",
                timeout.as_secs_f64()
            );
        }
        let poll = deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(100));
        match tokio::time::timeout(poll, watch.next()).await {
            Ok(Some(Ok(entry))) if entry.operation == kv::Operation::Put => {
                let registration: HookRegistration = serde_json::from_slice(&entry.value)
                    .with_context(|| format!("decode hook registration '{key}'"))?;
                if registration.server == server {
                    return Ok(());
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(error))) => {
                return Err(error).with_context(|| format!("watch hook registration '{key}'"));
            }
            Ok(None) => bail!("hook registration watch closed for '{key}'"),
            Err(_) => continue,
        }
    }
}

async fn ensure_bucket(client: &async_nats::Client, bucket: &str) -> Result<kv::Store> {
    let jetstream = jetstream::new(client.clone());
    match jetstream
        .create_key_value(kv::Config {
            bucket: bucket.to_string(),
            history: 1,
            num_replicas: 1,
            storage: stream::StorageType::File,
            ..Default::default()
        })
        .await
    {
        Ok(store) => Ok(store),
        Err(_) => jetstream
            .get_key_value(bucket)
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("open hook KV bucket '{bucket}'")),
    }
}

async fn publish_expectation(
    store: &kv::Store,
    key: &str,
    server: &str,
    hook: &HookConfig,
) -> Result<()> {
    let registration = HookRegistration {
        server: server.to_string(),
        hooks: vec![HookSpec {
            event: hook.event.clone(),
            matcher: hook.matcher.clone(),
            priority: 0,
            timeout_secs: hook.timeout,
            fail_policy: FailPolicy::Closed,
        }],
        schema_version: HOOK_SCHEMA_VERSION,
        proto_version: HOOK_PROTOCOL_VERSION,
    };
    let payload = serde_json::to_vec(&registration).context("encode hook expectation")?;
    store
        .put(key.to_string(), payload.into())
        .await
        .with_context(|| format!("publish fail-closed hook expectation '{key}'"))?;
    Ok(())
}

async fn remove_registration(client: &async_nats::Client, instance_id: &InstanceId, server: &str) {
    let jetstream = jetstream::new(client.clone());
    let Ok(store) = jetstream.get_key_value(HOOK_REGISTRY_BUCKET).await else {
        return;
    };
    let _ = store
        .delete(hook_registration_key(instance_id, server))
        .await;
}

async fn remove_registration_and_expectation(
    client: &async_nats::Client,
    instance_id: &InstanceId,
    server: &str,
) {
    let jetstream = jetstream::new(client.clone());
    let key = hook_registration_key(instance_id, server);
    for bucket in [HOOK_REGISTRY_BUCKET, HOOK_EXPECTATIONS_BUCKET] {
        let Ok(store) = jetstream.get_key_value(bucket).await else {
            continue;
        };
        let _ = store.delete(key.clone()).await;
    }
}

#[cfg(unix)]
fn configure_hook_process(command: &mut Command) {
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
fn configure_hook_process(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_proxy_auth_and_forwards_flags() {
        let package_dir = Path::new("/packages/coding");
        assert_eq!(
            proxy_auth_args(
                "harnx-proxy-auth --hook 'token helper' --hook '$HARNX_PACKAGE_DIR/hooks/jira.py' --env '$temp_file_root'",
                package_dir,
            )
            .unwrap(),
            Some(vec![
                "--hook".to_string(),
                "token helper".to_string(),
                "--hook".to_string(),
                "/packages/coding/hooks/jira.py".to_string(),
                "--env".to_string(),
                "$temp_file_root".to_string(),
            ])
        );
        assert_eq!(proxy_auth_args("echo hello", package_dir).unwrap(), None);
    }

    #[test]
    fn names_are_stable_and_nats_safe() {
        let hook = HookConfig {
            event: "PreToolUse".to_string(),
            matcher: None,
            command: "echo".to_string(),
            timeout: Some(30),
            status_message: None,
            async_hook: None,
            hook_type: "claude-command".to_string(),
            package_dir: None,
        };
        assert_eq!(
            hook_server_name("agent:review", 2, &hook),
            "agent-review-PreToolUse-2"
        );
    }
}
