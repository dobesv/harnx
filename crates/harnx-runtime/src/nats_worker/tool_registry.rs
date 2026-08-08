//! Tool-server identity and the JetStream KV registry they announce through.
//!
//! Split out of `tool_supervisor` so process supervision (spawning, monitoring,
//! restarting children) stays separate from registry bookkeeping (watching keys,
//! validating identities, cleaning up entries).

use crate::nats_tool_provider::NatsInFlightCalls;
use anyhow::{bail, Context, Result};
use async_nats::jetstream::{self, kv, stream};
use futures_util::{StreamExt, TryStreamExt};
use harnx_core::instance::InstanceId;
use harnx_toolset::{server_identity_token, Registration};
use harnx_toolset_server::{registration_key, TOOL_REGISTRY_BUCKET};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Identifies a supervised child by the pair that makes it unique. The bare
/// config name is ambiguous: two packages may each define a server called `fs`,
/// and treating them as one makes a live child mask its dead namesake.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct SupervisedServer {
    pub(super) package: Option<String>,
    pub(super) config: String,
}

impl SupervisedServer {
    pub(super) fn new(package: Option<&str>, config: &str) -> Self {
        Self {
            package: package.map(str::to_string),
            config: config.to_string(),
        }
    }

    pub(super) fn label(&self) -> String {
        match &self.package {
            Some(package) => format!("{package}/{}", self.config),
            None => self.config.clone(),
        }
    }
}

pub(super) type SupervisedProcesses = Arc<Mutex<HashMap<u32, SupervisedServer>>>;

/// Everything [`wait_for_registration`] needs about the server it is awaiting.
pub(super) struct RegistrationWait<'a> {
    pub(super) processes: &'a SupervisedProcesses,
    pub(super) identity: SupervisedServer,
    pub(super) instance_id: &'a InstanceId,
    pub(super) key: &'a str,
    pub(super) deadline: Instant,
    pub(super) timeout: Duration,
}

pub(super) async fn ensure_registry_bucket(
    client: &async_nats::Client,
    replicas: usize,
) -> Result<kv::Store> {
    let jetstream = jetstream::new(client.clone());
    match jetstream
        .create_key_value(kv::Config {
            bucket: TOOL_REGISTRY_BUCKET.to_string(),
            history: 1,
            num_replicas: replicas,
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

/// Dump every key in the tool registry when servers fail to register.
///
/// Distinguishes the two failure shapes that look identical from the outside: a
/// server that never published anything, versus one that published under a key
/// the watcher does not match.
pub(super) async fn log_registry_contents(registry: &kv::Store, instance_id: &InstanceId) {
    let keys = match registry.keys().await {
        Ok(keys) => keys.try_collect::<Vec<_>>().await,
        Err(error) => {
            log::warn!("could not list tool registry keys: {error:#}");
            return;
        }
    };
    match keys {
        Ok(keys) => {
            let prefix = format!("{instance_id}.");
            let (mine, others): (Vec<_>, Vec<_>) =
                keys.iter().partition(|key| key.starts_with(&prefix));
            log::warn!(
                "tool registry for instance {instance_id}: {} key(s) {:?}; {} key(s) from other instances",
                mine.len(),
                mine,
                others.len()
            );
        }
        Err(error) => log::warn!("could not read tool registry keys: {error:#}"),
    }
}

/// Wait until the server behind `wait` publishes a registration matching its
/// identity, its process dies, or the deadline passes.
pub(super) async fn wait_for_registration(
    watch: &mut kv::Watch,
    wait: RegistrationWait<'_>,
) -> Result<()> {
    loop {
        let poll = next_poll_interval(&wait).await?;
        if watched_entry_completes(watch, poll, &wait).await {
            return Ok(());
        }
    }
}

/// How long to block on the watch before re-checking liveness, or an error when
/// the server has died or the deadline has passed.
async fn next_poll_interval(wait: &RegistrationWait<'_>) -> Result<Duration> {
    if !is_still_running(wait.processes, &wait.identity).await {
        bail!("exited before registering at '{}'", wait.key);
    }
    let now = Instant::now();
    if now >= wait.deadline {
        // Its process is still alive, so this is "not ready yet", not "dead".
        // Say so and name the log, rather than reporting a slow server — an MCP
        // bridge waiting on a cold `npx` download routinely outlasts this — as a
        // failure the reader can do nothing about.
        bail!(
            "has not registered after {}s and is still running, so it may still be \
             starting; its output goes to {}",
            wait.timeout.as_secs_f64(),
            crate::local_orchestrator::local_worker_output_file().display()
        );
    }
    Ok(wait
        .deadline
        .saturating_duration_since(now)
        .min(Duration::from_millis(100)))
}

/// Take one step on the watch, reporting whether it delivered the registration
/// this wait is looking for. Anything else — a poll timeout, an unrelated key,
/// a watch error — is a reason to loop again, not to fail.
async fn watched_entry_completes(
    watch: &mut kv::Watch,
    poll: Duration,
    wait: &RegistrationWait<'_>,
) -> bool {
    match tokio::time::timeout(poll, watch.next()).await {
        Ok(Some(Ok(entry))) => {
            entry.operation == kv::Operation::Put && entry_completes_registration(&entry, wait)
        }
        Ok(Some(Err(error))) => {
            log::warn!(
                "ignoring tool registration watch error for '{}': {error}",
                wait.key
            );
            false
        }
        Ok(None) => {
            log::warn!(
                "tool registration watch closed for '{}'; waiting for timeout",
                wait.key
            );
            tokio::time::sleep(poll).await;
            false
        }
        Err(_) => false,
    }
}

async fn is_still_running(processes: &SupervisedProcesses, identity: &SupervisedServer) -> bool {
    processes.lock().await.values().any(|live| live == identity)
}

/// Whether a KV entry is the registration this wait is looking for. Mismatches
/// are logged and rejected rather than silently accepted, so a server announcing
/// the wrong identity is visible instead of quietly serving another's tools.
fn entry_completes_registration(entry: &kv::Entry, wait: &RegistrationWait<'_>) -> bool {
    if !entry.key.starts_with(wait.key.trim_end_matches("<server>")) {
        return false;
    }
    let registration: Registration = match serde_json::from_slice(&entry.value) {
        Ok(registration) => registration,
        Err(error) => {
            log::warn!(
                "ignoring invalid tool registration '{}': {error}",
                entry.key
            );
            return false;
        }
    };
    if registration.package != wait.identity.package || registration.config != wait.identity.config
    {
        log::warn!(
            "ignoring tool registration '{}' identity mismatch: expected package {:?}, config '{}'; got package {:?}, config '{}'",
            entry.key,
            wait.identity.package,
            wait.identity.config,
            registration.package,
            registration.config
        );
        return false;
    }
    let identity_token = server_identity_token(
        registration.package.as_deref(),
        &registration.config,
        &registration.server,
    );
    let expected_key = registration_key(wait.instance_id, &identity_token);
    if entry.key != expected_key {
        log::warn!(
            "ignoring tool registration '{}' because its identity expects key '{expected_key}'",
            entry.key
        );
        return false;
    }
    true
}

pub(super) async fn remove_registrations_for_config(
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
