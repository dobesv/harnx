//! Shared session attachment for CLI commands that answer or act on a
//! session from outside a frontend: `interact` (T4.2) and `surface` (T5.1).

use anyhow::{Context, Result};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::config::ConfigLock;
use harnx_runtime::nats_event_sink::SessionEventStream;
use harnx_runtime::{NatsSession, NatsSessionConfig};
use std::sync::Arc;

/// Attach the session the command targets, like a frontend would.
pub(crate) async fn attach_session(
    config: crate::config::Config,
    agent: &str,
    session: &str,
) -> Result<NatsSession> {
    let (agent, cluster) = config.resolve_session_agent(agent)?;
    let global = Arc::new(ConfigLock::new(config));
    NatsSession::from_global_config(
        NatsSessionConfig {
            cluster,
            initializer: harnx_runtime::SessionInitializer::named(agent, Default::default()),
            session_id: Some(session.to_string()),
            activation_route: harnx_runtime::nats_worker::SessionActivationRoute::ClusterShared,
        },
        &global,
        harnx_core::abort::create_abort_signal(),
    )
    .await
    .context("cannot attach to the session")
}

/// The session's durable log entries, refreshed from JetStream — the source
/// every command derives pending interactions and published surfaces from.
pub(crate) async fn session_entries(
    config: &crate::config::Config,
    agent: &str,
    session: &str,
) -> Result<Vec<(u64, SessionLogEntry)>> {
    let (jetstream, metadata) =
        harnx_runtime::config::session_metadata_for_agent(config, agent, session).await?;
    let (_, cluster) = config.resolve_session_agent(agent)?;
    let client = config.nats_client(&cluster).await?;
    let mut stream = SessionEventStream::attach(jetstream, client, &metadata.storage_key())
        .await
        .with_context(|| format!("cannot attach session log of '{session}' for '{agent}'"))?;
    stream
        .refresh_history()
        .await
        .context("cannot refresh session history")?;
    Ok(stream.history().to_vec())
}
