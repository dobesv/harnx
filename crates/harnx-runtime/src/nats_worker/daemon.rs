//! Worker daemon and session activation.

use super::daemon_background::launch_worker_services;
use super::daemon_runtime::WorkerRuntime;
use crate::config::{GlobalConfig, LOCAL_CLUSTER_KEY};
use crate::nats_lease::NatsSessionLease;
use crate::nats_session_metadata::SessionMetadataStore;
use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer::pull};
use futures_util::{Stream, StreamExt};
use std::collections::HashMap;
use std::fmt::Display;
use std::future::Future;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const ACTIVATION_STREAM_RETRY_DELAY: Duration = Duration::from_millis(250);

pub use super::activation::{
    new_remote_session_id, new_worker_id, SessionActivate, SessionActivationRoute,
};
use super::activation_transport::ensure_activation_consumer;
pub(super) use super::activation_transport::spawn_readiness_publisher;
pub use super::activation_transport::{
    notify_subject, publish_session_activate, publish_targeted_session_activate,
    targeted_consumer_name, targeted_notify_subject, targeted_worker_ready_subject,
    validate_worker_id, worker_ready_subject, LocalWorkerTarget,
};
pub use super::daemon_config::{
    resolve_worker_scope, NatsConnectionSource, WorkerActivationMode, WorkerDaemonConfig,
};

pub(crate) fn should_append_control_log_entry(lease: &NatsSessionLease) -> bool {
    lease.is_held()
}

pub(super) struct WorkerStartup {
    pub(super) jetstream: jetstream::Context,
    pub(super) client: async_nats::Client,
    pub(super) consumer: jetstream::consumer::Consumer<pull::Config>,
    /// JetStream replica count for the daemon connection, resolved once here so
    /// every bucket this worker creates (leases, session metadata, tool/hook
    /// registries) agrees on it instead of drifting to a per-site default.
    pub(super) replicas: usize,
    pub(super) identity: crate::worker_identity::WorkerReadiness,
}

async fn prepare_worker_startup(
    config: &GlobalConfig,
    daemon: &WorkerDaemonConfig,
) -> Result<WorkerStartup> {
    validate_worker_daemon(daemon)?;
    let identity =
        crate::worker_identity::WorkerReadiness::current(&daemon.session_scope, &daemon.worker_id);
    let (jetstream, client, replicas) = connect_worker(config, daemon).await?;
    let consumer = ensure_activation_consumer(&jetstream, daemon).await?;
    Ok(WorkerStartup {
        jetstream,
        client,
        consumer,
        replicas,
        identity,
    })
}

fn validate_worker_daemon(daemon: &WorkerDaemonConfig) -> Result<()> {
    if daemon.activation_mode == WorkerActivationMode::WorkerTargeted {
        validate_worker_id(&daemon.worker_id)?;
        anyhow::ensure!(
            daemon.session_scope == LOCAL_CLUSTER_KEY,
            "targeted workers currently require session scope {LOCAL_CLUSTER_KEY}"
        );
        anyhow::ensure!(
            daemon.connection == NatsConnectionSource::LocalEnvironment,
            "targeted local workers require the local NATS environment handoff"
        );
        anyhow::ensure!(
            daemon.manage_servers,
            "targeted local workers require managed tool and hook servers"
        );
    }
    Ok(())
}

async fn connect_worker(
    config: &GlobalConfig,
    daemon: &WorkerDaemonConfig,
) -> Result<(jetstream::Context, async_nats::Client, usize)> {
    let (jetstream, client, replicas) = {
        let cfg = config.read().clone();
        let connection_key = daemon.connection_key();
        let jetstream = cfg.nats_jetstream(connection_key).await?;
        let client = cfg.nats_client(connection_key).await?;
        let replicas = cfg.resolve_nats_server(connection_key).await?.replicas;
        (jetstream, client, replicas.unwrap_or(1))
    };
    Ok((jetstream, client, replicas))
}

pub(super) async fn ensure_session_metadata(
    jetstream: &jetstream::Context,
    replicas: usize,
) -> Result<SessionMetadataStore> {
    SessionMetadataStore::ensure(jetstream, replicas)
        .await
        .context("failed to ensure canonical harnx_sessions metadata bucket")
}

fn load_session_agent(
    per_session: &GlobalConfig,
    metadata: &crate::nats_session_metadata::SessionMetadata,
) -> Result<crate::config::Agent> {
    use crate::nats_session_metadata::SessionAgentSource;

    let mut agent = match &metadata.agent {
        SessionAgentSource::Named { name } => {
            let mut agent = per_session
                .read()
                .retrieve_agent(name)
                .with_context(|| format!("load named session agent '{name}'"))?;
            crate::config::agent::resolve_file_defaults(&mut agent)
                .with_context(|| format!("resolve file-backed variables for agent '{name}'"))?;
            let variables = crate::config::agent::require_agent_variables(
                agent.defined_variables(),
                &metadata.variables,
            )
            .with_context(|| format!("initialize variables for agent '{name}'"))?;
            agent.set_shared_variables(variables);
            agent
        }
        SessionAgentSource::Inline { instructions } => crate::config::Agent::new(
            harnx_core::agent_config::AgentConfig::from_prompt(instructions),
        ),
    };
    if agent.defined_variables().is_empty() {
        agent.set_shared_variables(metadata.variables.clone());
    }
    Ok(agent)
}

fn apply_session_overrides(
    config: &mut crate::config::Config,
    overrides: &crate::nats_session_metadata::SessionOverrides,
) -> Result<()> {
    apply_model_overrides(config, overrides)?;
    apply_tool_overrides(config, overrides)?;
    apply_limit_overrides(config, overrides)
}

fn apply_model_overrides(
    config: &mut crate::config::Config,
    overrides: &crate::nats_session_metadata::SessionOverrides,
) -> Result<()> {
    if let Some(model) = &overrides.model {
        config.set_model(model)?;
    }
    if let Some(temperature) = overrides.temperature {
        config.set_temperature(Some(temperature));
    }
    if let Some(top_p) = overrides.top_p {
        config.set_top_p(Some(top_p));
    }
    Ok(())
}

fn apply_tool_overrides(
    config: &mut crate::config::Config,
    overrides: &crate::nats_session_metadata::SessionOverrides,
) -> Result<()> {
    if let Some(use_tools) = &overrides.use_tools {
        config.set_use_tools(Some(use_tools.clone()));
    }
    if !overrides.model_fallbacks.is_empty() {
        config.set_model_fallbacks(overrides.model_fallbacks.clone());
    }
    Ok(())
}

fn apply_limit_overrides(
    config: &mut crate::config::Config,
    overrides: &crate::nats_session_metadata::SessionOverrides,
) -> Result<()> {
    if let Some(threshold) = overrides.compress_threshold {
        config.set_compress_threshold(Some(threshold));
    }
    if let Some(compaction_agent) = &overrides.compaction_agent {
        config.set_compaction_agent(Some(compaction_agent.clone()));
    }
    if let Some(max_output_tokens) = overrides.max_output_tokens {
        config.set_max_output_tokens(Some(max_output_tokens));
    }
    Ok(())
}

/// Install the activation's agent into the per-session config.
///
/// Mirrors the local `use_agent_by_name` flow: file-backed variable defaults
/// (`variables: [{name, path}]`) must be read off disk and folded into the
/// agent's shared variables before anything renders the prompt template, or
/// an agent like `pantheon/sisyphus` fails with an undefined value on its
/// first render.
///
/// A missing or invalid named agent is fatal; workers never substitute their
/// own active configuration for the session's immutable identity.
///
/// `pub(super)`: also used by `server_reconciler::tool_servers_for_activation`
/// to resolve which servers a session's agent needs, on a throwaway config
/// clone, before the real per-session config below is built.
pub(super) fn install_session_metadata_agent(
    per_session: &GlobalConfig,
    metadata: &crate::nats_session_metadata::SessionMetadata,
) -> Result<()> {
    let agent = load_session_agent(per_session, metadata)?;
    let mut cfg = per_session.write();
    cfg.use_agent_obj(agent).context("activate session agent")?;
    apply_session_overrides(&mut cfg, &metadata.overrides)
}

/// Run a worker daemon: pull `SessionActivate` notifications, claim each via a
/// KV lease, and execute the session (exactly one worker per session).
pub async fn run_worker_daemon(
    config: GlobalConfig,
    mut daemon: WorkerDaemonConfig,
    call_fn: Option<crate::agent_loop::AgentCallFn>,
) -> Result<()> {
    let instance_id = resolve_worker_scope(daemon.manage_servers)?;
    let startup = prepare_worker_startup(&config, &daemon).await?;
    log::info!(
        "serving under server_scope='{}' session_scope={} worker_id={} pid={} build={} activation={:?}",
        instance_id.as_str(),
        startup.identity.session_scope,
        startup.identity.worker_id,
        startup.identity.pid,
        startup.identity.build,
        daemon.activation_mode,
    );
    // The daemon config has no cluster to resolve against yet, so its lease
    // config carries the bare default; now that the connection source is
    // resolved, the lease bucket agrees with everything else this worker
    // creates instead of always sitting at the default.
    daemon.lease.replicas = startup.replicas;
    let services = launch_worker_services(&config, &daemon, &startup, &instance_id).await?;
    let runtime = Arc::new(WorkerRuntime {
        config,
        instance_id,
        _background_services: services.background,
        background_services_attempted: services.background_services_attempted,
        server_reconciler: services.server_reconciler,
        cluster: daemon.connection_key().to_string(),
        activation_route: daemon.activation_route(),
        activation_mode: daemon.activation_mode,
        manage_servers: daemon.manage_servers,
        worker_id: daemon.worker_id.clone(),
        identity: startup.identity.clone(),
        lease: daemon.lease,
        jetstream: startup.jetstream,
        session_metadata: services.session_metadata,
        client: startup.client,
        call_fn,
        generation: AtomicU64::new(1),
        active: Mutex::new(HashMap::new()),
    });

    let consumer = startup.consumer;
    reconnect_activation_stream(
        move || {
            let consumer = consumer.clone();
            async move { consumer.messages().await }
        },
        move |message| {
            let runtime = Arc::clone(&runtime);
            async move { runtime.handle_activation(message).await }
        },
        ACTIVATION_STREAM_RETRY_DELAY,
    )
    .await;
    Ok(())
}

/// Keep the daemon's pull subscription alive across transient broker stalls.
///
/// `async-nats` reports a missed idle heartbeat as an item error. The stream
/// may become usable again, but recreating it issues a fresh pull request and
/// also handles streams that closed while the client reconnected. An active
/// turn is independently protected by its lease; losing the activation stream
/// must not terminate the worker process and strand later reactivations.
async fn reconnect_activation_stream<
    Open,
    OpenFuture,
    Messages,
    Message,
    OpenError,
    MessageError,
    Handle,
    HandleFuture,
    HandleError,
>(
    mut open: Open,
    mut handle: Handle,
    retry_delay: Duration,
) where
    Open: FnMut() -> OpenFuture,
    OpenFuture: Future<Output = std::result::Result<Messages, OpenError>>,
    Messages: Stream<Item = std::result::Result<Message, MessageError>> + Unpin,
    OpenError: Display,
    MessageError: Display,
    Handle: FnMut(Message) -> HandleFuture,
    HandleFuture: Future<Output = std::result::Result<(), HandleError>>,
    HandleError: Display,
{
    loop {
        match open().await {
            Ok(mut messages) => {
                while let Some(message) = messages.next().await {
                    let message = match message {
                        Ok(message) => message,
                        Err(error) => {
                            log::warn!("worker activation stream failed; reopening: {error}");
                            break;
                        }
                    };
                    if let Err(error) = handle(message).await {
                        log::warn!("worker activation handling failed: {error}");
                    }
                }
            }
            Err(error) => {
                log::warn!("failed to open worker activation stream; retrying: {error}");
            }
        }
        tokio::time::sleep(retry_delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::reconnect_activation_stream;
    use futures_util::stream::{self, BoxStream};
    use futures_util::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn activation_stream_reopens_after_open_and_item_failures() {
        harnx_core::require_nextest();
        let attempts = Arc::new(AtomicUsize::new(0));
        let (handled_tx, handled_rx) = tokio::sync::oneshot::channel();
        let mut handled_tx = Some(handled_tx);

        let task = tokio::spawn(reconnect_activation_stream(
            {
                let attempts = Arc::clone(&attempts);
                move || {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    async move {
                        let messages: BoxStream<'static, std::result::Result<u32, &'static str>> =
                            match attempt {
                                1 => stream::iter([Err("missed idle heartbeat")]).boxed(),
                                _ => stream::once(async { Ok(42) })
                                    .chain(stream::pending())
                                    .boxed(),
                            };
                        if attempt == 0 {
                            Err("broker unavailable")
                        } else {
                            Ok(messages)
                        }
                    }
                }
            },
            move |message| {
                let tx = handled_tx.take();
                async move {
                    if let Some(tx) = tx {
                        let _ = tx.send(message);
                    }
                    Ok::<(), &'static str>(())
                }
            },
            Duration::from_millis(1),
        ));

        let message = tokio::time::timeout(Duration::from_secs(1), handled_rx)
            .await
            .expect("activation stream did not recover")
            .expect("activation handler disappeared");
        assert_eq!(message, 42);
        assert!(attempts.load(Ordering::SeqCst) >= 3);
        assert!(!task.is_finished(), "recovered daemon loop must stay alive");

        task.abort();
        let _ = task.await;
    }
}
