use crate::config::{HARNX_NATS_REPLICAS_ENV, HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV};
use anyhow::{bail, Context, Result};
use async_nats::jetstream::{self, kv, stream};
use futures_util::StreamExt;
use harnx_core::hooks::{HookConfig, HooksConfig};
use harnx_core::instance::{InstanceId, HARNX_INSTANCE_ID};
use harnx_hooks::executor::HARNX_PACKAGE_DIR_ENV;
use harnx_hookset::{
    FailPolicy, HookRegistration, HookSpec, HARNX_HOOK_NAME, HOOK_EXPECTATIONS_BUCKET,
    HOOK_PROTOCOL_VERSION, HOOK_REGISTRY_BUCKET, HOOK_SCHEMA_VERSION,
};
use harnx_hookset_server::hook_registration_key;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

const HOOK_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HOOKS_PER_SUPERVISOR: usize = 999;

/// Connection and identity shared by hook servers spawned for one worker.
#[derive(Clone)]
pub struct HookServerStartConfig {
    client: async_nats::Client,
    instance_id: InstanceId,
    nats_url: String,
    token: String,
    /// JetStream replica count for buckets this cluster's hook servers
    /// create. `None` means 1; see `docs/nats-ha.md`.
    replicas: Option<usize>,
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
            replicas: None,
        }
    }

    /// Set the JetStream replica count from the cluster this config connects to.
    pub fn with_replicas(mut self, replicas: Option<usize>) -> Self {
        self.replicas = replicas;
        self
    }

    /// The replica count to actually use: the configured value, or 1 when unset.
    fn resolved_replicas(&self) -> usize {
        self.replicas.unwrap_or(1)
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
        _scope: &str,
        readiness_timeout: Duration,
    ) -> Result<Self> {
        let run_id = Uuid::new_v4().simple().to_string()[..8].to_string();
        let processes = Arc::new(Mutex::new(HashMap::new()));
        let mut supervisor = Self {
            processes: Arc::clone(&processes),
            tasks: Vec::new(),
            client: config.client.clone(),
            instance_id: config.instance_id.clone(),
            registrations: Vec::new(),
        };
        let enabled: Vec<_> = hooks.entries.iter().collect();
        validate_hook_count(enabled.len())?;
        if enabled.is_empty() {
            return Ok(supervisor);
        }
        let launch_plan: Vec<_> = enabled
            .into_iter()
            .enumerate()
            .map(|(order_index, hook)| HookLaunch {
                order_index,
                name: hook_server_name(&run_id, order_index),
                hook,
            })
            .collect();

        let prepared = prepare_hook_registrations(&config, &launch_plan, &mut supervisor).await?;
        let startups = spawn_enabled_hooks(&config, prepared, &mut supervisor).await?;
        let monitors =
            start_hook_servers(startups, &config, Arc::clone(&processes), readiness_timeout)
                .await?;
        supervisor.tasks.extend(monitors);
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

fn validate_hook_count(count: usize) -> Result<()> {
    if count > MAX_HOOKS_PER_SUPERVISOR {
        bail!("hook supervisor supports at most {MAX_HOOKS_PER_SUPERVISOR} hooks, got {count}");
    }
    Ok(())
}

struct HookLaunch<'a> {
    order_index: usize,
    name: String,
    hook: &'a HookConfig,
}

struct PreparedHook {
    order_index: usize,
    name: String,
    key: String,
    rejector_name: String,
    display_label: String,
    failure_label: String,
    hook: HookConfig,
    watch: kv::Watch,
}

struct HookStartup {
    name: String,
    key: String,
    /// Untruncated command, for diagnosing a server that dies before it registers.
    hook_command: String,
    rejector_name: String,
    display_label: String,
    failure_label: String,
    watch: kv::Watch,
    child: Child,
    pid: u32,
}

struct HookMonitor {
    child: Child,
    pid: u32,
    server: String,
    display_label: String,
    registration: HookRegistration,
    instance_id: InstanceId,
    client: async_nats::Client,
    processes: Arc<Mutex<HashMap<u32, String>>>,
}

async fn prepare_hook_registrations(
    config: &HookServerStartConfig,
    launch_plan: &[HookLaunch<'_>],
    supervisor: &mut HookServerSupervisor,
) -> Result<Vec<PreparedHook>> {
    let registry = ensure_bucket_for(config, HOOK_REGISTRY_BUCKET).await?;
    let expectations = ensure_bucket_for(config, HOOK_EXPECTATIONS_BUCKET).await?;
    let mut prepared = Vec::new();
    for launch in launch_plan {
        let key = hook_registration_key(&config.instance_id, &launch.name);
        let rejector_name = format!("{}-rejector", launch.name);
        supervisor.registrations.push(launch.name.clone());
        supervisor.registrations.push(rejector_name.clone());
        let display_label = hook_display_label(launch.hook);
        let failure_label = startup_failure_label(launch.hook);

        if let Err(error) = registry.delete(&key).await {
            publish_startup_rejector(
                &expectations,
                &config.instance_id,
                &rejector_name,
                &failure_label,
            )
            .await?;
            log::warn!(
                "hook server '{}' startup rejected because stale registration '{}' could not be deleted: {error:#}",
                launch.name,
                key
            );
            continue;
        }

        let watch = match registry.watch_with_history(&key).await {
            Ok(watch) => watch,
            Err(error) => {
                publish_startup_rejector(
                    &expectations,
                    &config.instance_id,
                    &rejector_name,
                    &failure_label,
                )
                .await?;
                log::warn!(
                    "hook server '{}' registration watch failed: {error:#}",
                    launch.name
                );
                continue;
            }
        };
        prepared.push(PreparedHook {
            order_index: launch.order_index,
            name: launch.name.clone(),
            key,
            rejector_name,
            display_label,
            failure_label,
            hook: launch.hook.clone(),
            watch,
        });
    }
    Ok(prepared)
}

async fn spawn_enabled_hooks(
    config: &HookServerStartConfig,
    prepared: Vec<PreparedHook>,
    supervisor: &mut HookServerSupervisor,
) -> Result<Vec<HookStartup>> {
    let expectations = ensure_bucket_for(config, HOOK_EXPECTATIONS_BUCKET).await?;
    let mut startups = Vec::new();
    for prepared in prepared {
        let mut child = match spawn_hook_server(config, &prepared.hook, &prepared.name) {
            Ok(child) => child,
            Err(error) => {
                publish_startup_rejector(
                    &expectations,
                    &config.instance_id,
                    &prepared.rejector_name,
                    &prepared.failure_label,
                )
                .await?;
                log::warn!(
                    "hook server #{index} '{name}' failed to spawn: {error:#}",
                    index = prepared.order_index,
                    name = prepared.name
                );
                continue;
            }
        };
        let Some(pid) = child.id() else {
            let _ = child.start_kill();
            publish_startup_rejector(
                &expectations,
                &config.instance_id,
                &prepared.rejector_name,
                &prepared.failure_label,
            )
            .await?;
            log::warn!("hook server '{}' has no process ID", prepared.name);
            continue;
        };
        supervisor
            .processes
            .lock()
            .await
            .insert(pid, prepared.name.clone());
        startups.push(HookStartup {
            name: prepared.name,
            key: prepared.key,
            hook_command: prepared.hook.command.clone(),
            rejector_name: prepared.rejector_name,
            display_label: prepared.display_label,
            failure_label: prepared.failure_label,
            watch: prepared.watch,
            child,
            pid,
        });
    }
    Ok(startups)
}

async fn start_hook_servers(
    startups: Vec<HookStartup>,
    config: &HookServerStartConfig,
    processes: Arc<Mutex<HashMap<u32, String>>>,
    readiness_timeout: Duration,
) -> Result<Vec<JoinHandle<()>>> {
    let starts = startups.into_iter().map(|startup| {
        let config = config.clone();
        let processes = Arc::clone(&processes);
        async move { start_hook_server(startup, config, processes, readiness_timeout).await }
    });
    let mut monitors = Vec::new();
    for result in futures_util::future::join_all(starts).await {
        if let Some(monitor) = result? {
            monitors.push(monitor);
        }
    }
    Ok(monitors)
}

async fn start_hook_server(
    startup: HookStartup,
    config: HookServerStartConfig,
    processes: Arc<Mutex<HashMap<u32, String>>>,
    readiness_timeout: Duration,
) -> Result<Option<JoinHandle<()>>> {
    let HookStartup {
        name,
        key,
        hook_command,
        rejector_name,
        display_label,
        failure_label,
        mut watch,
        mut child,
        pid,
    } = startup;

    let readiness = tokio::select! {
        registration = wait_for_registration(&mut watch, &name, &key) => registration,
        status = child.wait() => match status {
            Ok(status) => Err(anyhow::anyhow!(
                "hook server '{name}' exited with {status} before registering at '{key}'"
            )),
            Err(error) => Err(error).with_context(|| format!("wait for hook server '{name}' during startup")),
        },
        _ = tokio::time::sleep(readiness_timeout) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(anyhow::anyhow!(
                "hook server '{name}' did not register at '{key}' within {}s",
                readiness_timeout.as_secs_f64()
            ))
        }
    };

    match readiness {
        Ok(registration) => Ok(Some(spawn_child_monitor(HookMonitor {
            child,
            pid,
            server: name,
            display_label,
            registration,
            instance_id: config.instance_id,
            client: config.client,
            processes,
        }))),
        Err(error) => {
            processes.lock().await.remove(&pid);
            let _ = child.start_kill();
            let _ = child.wait().await;
            let expectations = ensure_bucket_for(&config, HOOK_EXPECTATIONS_BUCKET).await?;
            publish_startup_rejector(
                &expectations,
                &config.instance_id,
                &rejector_name,
                &failure_label,
            )
            .await?;
            // `display_label` is capped at 120 chars for the UI, which cuts the
            // command mid-flag and hides the argument that actually failed.
            log::warn!(
                "hook server '{name}' failed during startup: {error:#}; command: {}",
                hook_command
            );
            Ok(None)
        }
    }
}

fn hook_server_name(run_id: &str, order_index: usize) -> String {
    format!("hook-{run_id}-{order_index:03}")
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
    let mut words = shell_words::split(&hook.command).context("parse hook command")?;
    if words.is_empty() {
        bail!("hook command is empty");
    }
    let package_dir_value = package_dir.to_string_lossy();
    for word in &mut words[1..] {
        *word = word
            .replace("${HARNX_PACKAGE_DIR}", &package_dir_value)
            .replace("$HARNX_PACKAGE_DIR", &package_dir_value);
    }
    let binary = resolve_binary(&words[0])?;
    let mut command = Command::new(binary);
    command.args(&words[1..]);

    command
        .env(HARNX_PACKAGE_DIR_ENV, package_dir)
        .env(HARNX_HOOK_NAME, name)
        .env(HARNX_INSTANCE_ID, config.instance_id.as_str())
        .env(HARNX_NATS_URL_ENV, &config.nats_url)
        .env(HARNX_NATS_TOKEN_ENV, &config.token)
        .env(
            HARNX_NATS_REPLICAS_ENV,
            config.resolved_replicas().to_string(),
        )
        .stdin(Stdio::null())
        // Send output to the worker log so a hook server that exits before
        // registering explains itself instead of failing silently.
        .stdout(crate::local_orchestrator::worker_output_sink())
        .stderr(crate::local_orchestrator::worker_output_sink())
        .kill_on_drop(true);
    configure_hook_process(&mut command);
    command
        .spawn()
        .with_context(|| format!("spawn hook server '{name}'"))
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

fn spawn_child_monitor(monitor: HookMonitor) -> JoinHandle<()> {
    tokio::spawn(async move {
        let HookMonitor {
            mut child,
            pid,
            server,
            display_label,
            registration,
            instance_id,
            client,
            processes,
        } = monitor;
        let status = child.wait().await;
        processes.lock().await.remove(&pid);
        replace_crashed_hook_route(
            &client,
            &instance_id,
            &server,
            crash_marker(registration, display_label),
        )
        .await;
        log_child_exit(&server, status);
    })
}

fn crash_marker(mut registration: HookRegistration, display_label: String) -> HookRegistration {
    registration.display_label = Some(display_label);
    for hook in &mut registration.hooks {
        hook.fail_policy = FailPolicy::Closed;
    }
    registration
}

async fn replace_crashed_hook_route(
    client: &async_nats::Client,
    instance_id: &InstanceId,
    server: &str,
    marker: HookRegistration,
) {
    let key = hook_registration_key(instance_id, server);
    let rejector_name = format!("{server}-rejector");
    let rejector_label: String = format!(
        "hook server crashed: {}",
        marker.display_label.as_deref().unwrap_or(server)
    )
    .chars()
    .take(120)
    .collect();

    // Publish before deleting the live registration so discovery never sees
    // an empty route set. If both publications fail, retain the live route.
    let route = publish_marker_or_rejector(
        || async {
            let expectations = ensure_bucket(client, HOOK_EXPECTATIONS_BUCKET, 1).await?;
            publish_registration(&expectations, &key, &marker).await
        },
        || publish_crash_rejector(client, instance_id, &rejector_name, &rejector_label),
    )
    .await;
    finish_crash_route(
        CrashRouteContext {
            client,
            instance_id,
            server,
            rejector_name: &rejector_name,
        },
        route,
    )
    .await;
}

struct CrashRouteContext<'a> {
    client: &'a async_nats::Client,
    instance_id: &'a InstanceId,
    server: &'a str,
    rejector_name: &'a str,
}

async fn finish_crash_route(context: CrashRouteContext<'_>, route: Result<CrashRoute>) {
    let CrashRouteContext {
        client,
        instance_id,
        server,
        rejector_name,
    } = context;
    match route {
        Ok(CrashRoute::Marker) => remove_registration(client, instance_id, server).await,
        Ok(CrashRoute::Rejector) => {
            log::warn!("hook server '{server}' crash marker failed; installed fail-closed rejector '{rejector_name}'");
            remove_registration(client, instance_id, server).await;
        }
        Err(error) => {
            log::error!("failed to install any fail-closed route for crashed hook server '{server}': {error:#}");
        }
    }
}

fn log_child_exit(server: &str, status: std::io::Result<std::process::ExitStatus>) {
    match status {
        Ok(status) if status.success() => log::debug!("hook server '{server}' exited"),
        Ok(status) => log::warn!("hook server '{server}' exited with {status}"),
        Err(error) => log::warn!("hook server '{server}' wait failed: {error}"),
    }
}

struct RegistrationExpectation<'a> {
    server: &'a str,
    key: &'a str,
}

async fn wait_for_registration(
    watch: &mut kv::Watch,
    server: &str,
    key: &str,
) -> Result<HookRegistration> {
    loop {
        let Some(entry) = watch.next().await else {
            bail!("hook registration watch closed for '{key}'");
        };
        let entry = entry.with_context(|| format!("watch hook registration '{key}'"))?;
        if entry.operation != kv::Operation::Put {
            continue;
        }
        return decode_expected_registration(&entry.value, RegistrationExpectation { server, key });
    }
}

fn decode_expected_registration(
    value: &[u8],
    expected: RegistrationExpectation<'_>,
) -> Result<HookRegistration> {
    let registration: HookRegistration = serde_json::from_slice(value)
        .with_context(|| format!("decode hook registration '{}'", expected.key))?;
    if registration.server != expected.server {
        bail!(
            "hook registration '{}' declared server '{}' instead of assigned name '{}'",
            expected.key,
            registration.server,
            expected.server
        );
    }
    Ok(registration)
}

/// `ensure_bucket` for the common case of a bucket that belongs to a
/// configured cluster, so call sites don't have to spell out
/// `&config.client` and `config.resolved_replicas()` every time.
async fn ensure_bucket_for(config: &HookServerStartConfig, bucket: &str) -> Result<kv::Store> {
    ensure_bucket(&config.client, bucket, config.resolved_replicas()).await
}

async fn ensure_bucket(
    client: &async_nats::Client,
    bucket: &str,
    replicas: usize,
) -> Result<kv::Store> {
    let jetstream = jetstream::new(client.clone());
    match jetstream
        .create_key_value(kv::Config {
            bucket: bucket.to_string(),
            history: 1,
            num_replicas: replicas,
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

fn hook_display_label(hook: &HookConfig) -> String {
    hook.status_message
        .as_deref()
        .unwrap_or(&hook.command)
        .chars()
        .take(120)
        .collect()
}

fn startup_failure_label(hook: &HookConfig) -> String {
    format!("hook server failed to start: {}", hook_display_label(hook))
        .chars()
        .take(120)
        .collect()
}

fn fail_closed_rejector(server: &str, display_label: &str) -> HookRegistration {
    HookRegistration {
        server: server.to_string(),
        display_label: Some(display_label.to_string()),
        hooks: vec![
            HookSpec {
                event: "UserPromptSubmit".to_string(),
                matcher: None,
                priority: 0,
                timeout_secs: None,
                fail_policy: FailPolicy::Closed,
            },
            HookSpec {
                event: "PreToolUse".to_string(),
                matcher: Some(".*".to_string()),
                priority: 0,
                timeout_secs: None,
                fail_policy: FailPolicy::Closed,
            },
        ],
        schema_version: HOOK_SCHEMA_VERSION,
        proto_version: HOOK_PROTOCOL_VERSION,
    }
}

async fn publish_startup_rejector(
    store: &kv::Store,
    instance_id: &InstanceId,
    server: &str,
    display_label: &str,
) -> Result<()> {
    let registration = fail_closed_rejector(server, display_label);
    let key = hook_registration_key(instance_id, server);
    publish_registration(store, &key, &registration).await
}

#[derive(Debug, Eq, PartialEq)]
enum CrashRoute {
    Marker,
    Rejector,
}

async fn publish_marker_or_rejector<M, MFut, R, RFut>(
    publish_marker: M,
    publish_rejector: R,
) -> Result<CrashRoute>
where
    M: FnOnce() -> MFut,
    MFut: std::future::Future<Output = Result<()>>,
    R: FnOnce() -> RFut,
    RFut: std::future::Future<Output = Result<()>>,
{
    match publish_marker().await {
        Ok(()) => Ok(CrashRoute::Marker),
        Err(marker_error) => {
            publish_rejector().await.with_context(|| {
                format!("crash marker failed ({marker_error:#}); publish crash rejector")
            })?;
            Ok(CrashRoute::Rejector)
        }
    }
}

/// Publishes a synthetic fail-closed route after a hook server crash.
///
/// Exposed for integration testing of the live JetStream fallback paths.
#[doc(hidden)]
pub async fn publish_crash_rejector(
    client: &async_nats::Client,
    instance_id: &InstanceId,
    server: &str,
    display_label: &str,
) -> Result<()> {
    let registration = fail_closed_rejector(server, display_label);
    let key = hook_registration_key(instance_id, server);

    let expectation_result = async {
        let expectations = ensure_bucket(client, HOOK_EXPECTATIONS_BUCKET, 1).await?;
        publish_registration(&expectations, &key, &registration).await
    }
    .await;
    if expectation_result.is_ok() {
        return Ok(());
    }

    // If the expectations path itself is unavailable, the registry bucket is a
    // second fail-closed publication path. The rejector still has no responder.
    let registry = ensure_bucket(client, HOOK_REGISTRY_BUCKET, 1).await?;
    publish_registration(&registry, &key, &registration)
        .await
        .with_context(|| format!("publish crash rejector after {expectation_result:#?}"))
}

async fn publish_registration(
    store: &kv::Store,
    key: &str,
    registration: &HookRegistration,
) -> Result<()> {
    let payload = serde_json::to_vec(registration).context("encode hook expectation")?;
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
    fn hook_count_limit_accepts_boundary_and_rejects_overflow() {
        validate_hook_count(MAX_HOOKS_PER_SUPERVISOR).expect("999 hooks are supported");
        let error = validate_hook_count(MAX_HOOKS_PER_SUPERVISOR + 1)
            .expect_err("1000 hooks must be rejected");
        assert_eq!(
            error.to_string(),
            "hook supervisor supports at most 999 hooks, got 1000"
        );
    }

    #[tokio::test]
    async fn crash_marker_failure_invokes_fail_closed_rejector_fallback() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let rejector_called = AtomicBool::new(false);
        let route = publish_marker_or_rejector(
            || async { anyhow::bail!("marker unavailable") },
            || async {
                rejector_called.store(true, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .expect("rejector fallback succeeds");

        assert_eq!(route, CrashRoute::Rejector);
        assert!(rejector_called.load(Ordering::SeqCst));
    }

    #[test]
    fn ordered_nonce_names_are_zero_padded() {
        assert_eq!(hook_server_name("a1b2c3d4", 0), "hook-a1b2c3d4-000");
        assert_eq!(hook_server_name("a1b2c3d4", 12), "hook-a1b2c3d4-012");
        assert_eq!(hook_server_name("a1b2c3d4", 998), "hook-a1b2c3d4-998");
        assert!(hook_server_name("a1b2c3d4", 2) < hook_server_name("a1b2c3d4", 10));
    }
}
