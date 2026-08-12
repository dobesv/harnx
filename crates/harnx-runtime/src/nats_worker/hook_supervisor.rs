use super::hook_crash::{publish_startup_rejector, RejectorTarget};
use super::hook_process::{spawn_child_monitor, spawn_hook_server, HookMonitor};
use super::hook_registration::{
    ensure_bucket_for, prepare_hook_registrations, remove_registration_and_expectation,
    wait_for_registration, PreparedHook,
};
use anyhow::{bail, Context, Result};
use async_nats::jetstream::kv;
use harnx_core::hooks::{HookConfig, HooksConfig};
use harnx_core::instance::ServerScope;
use harnx_hookset::HOOK_EXPECTATIONS_BUCKET;
use harnx_nats_common::connect::NatsEndpoint;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Child;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

const HOOK_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HOOKS_PER_SUPERVISOR: usize = 999;

/// Connection and identity shared by hook servers spawned for one worker.
#[derive(Clone)]
pub struct HookServerStartConfig {
    pub(super) client: async_nats::Client,
    pub(super) instance_id: ServerScope,
    pub(super) nats_url: String,
    pub(super) token: String,
    /// JetStream replica count for buckets this cluster's hook servers
    /// create. `None` means 1; see `docs/nats-ha.md`.
    pub(super) replicas: Option<usize>,
    /// TLS/mTLS settings for `nats_url`, mirrored into spawned children's
    /// environment. `None`/absent means no TLS.
    pub(super) tls: Option<bool>,
    pub(super) tls_cert: Option<String>,
    pub(super) tls_key: Option<String>,
    pub(super) tls_ca: Option<String>,
}

impl HookServerStartConfig {
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
        }
    }

    /// Set the JetStream replica count from the cluster this config connects to.
    pub fn with_replicas(mut self, replicas: Option<usize>) -> Self {
        self.replicas = replicas;
        self
    }

    /// Copy TLS/mTLS settings from `endpoint` (built from the cluster this
    /// config connects to) so spawned hook servers are told to use the same
    /// ones. Only `endpoint`'s TLS fields are read. Without this, a worker
    /// discovering over TLS spawns hook children that connect plaintext and
    /// can never reach the broker.
    pub fn with_tls(mut self, endpoint: &NatsEndpoint) -> Self {
        self.tls = endpoint.tls;
        self.tls_cert = endpoint.tls_cert.clone();
        self.tls_key = endpoint.tls_key.clone();
        self.tls_ca = endpoint.tls_ca.clone();
        self
    }

    /// The replica count to actually use: the configured value, or 1 when unset.
    pub(super) fn resolved_replicas(&self) -> usize {
        self.replicas.unwrap_or(1)
    }
}

/// Owns one child process per configured hook and removes registrations on exit.
pub struct HookServerSupervisor {
    processes: Arc<Mutex<HashMap<u32, String>>>,
    tasks: Vec<JoinHandle<()>>,
    client: async_nats::Client,
    instance_id: ServerScope,
    pub(super) registrations: Vec<String>,
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

pub(super) struct HookLaunch<'a> {
    pub(super) order_index: usize,
    pub(super) name: String,
    pub(super) hook: &'a HookConfig,
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
                    RejectorTarget {
                        server: &prepared.rejector_name,
                        display_label: &prepared.failure_label,
                    },
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
                RejectorTarget {
                    server: &prepared.rejector_name,
                    display_label: &prepared.failure_label,
                },
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
                RejectorTarget {
                    server: &rejector_name,
                    display_label: &failure_label,
                },
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

    #[test]
    fn ordered_nonce_names_are_zero_padded() {
        assert_eq!(hook_server_name("a1b2c3d4", 0), "hook-a1b2c3d4-000");
        assert_eq!(hook_server_name("a1b2c3d4", 12), "hook-a1b2c3d4-012");
        assert_eq!(hook_server_name("a1b2c3d4", 998), "hook-a1b2c3d4-998");
        assert!(hook_server_name("a1b2c3d4", 2) < hook_server_name("a1b2c3d4", 10));
    }
}
