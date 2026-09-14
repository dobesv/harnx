use super::Config;
use crate::nats_session_log::NatsSessionLog;
use anyhow::{Context, Result};

pub async fn render_session_dump_for_agent(
    config: &Config,
    cluster: &str,
    session_id: &str,
    agent: &str,
) -> Result<String> {
    anyhow::ensure!(
        !agent.trim().is_empty() && agent != harnx_core::agent_config::TEMP_AGENT_NAME,
        "an explicit agent is required to load a session"
    );
    let storage_key = harnx_core::session_identity::session_key(Some(agent), session_id);
    let jetstream = config.nats_jetstream(cluster).await?;
    let metadata_store =
        crate::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1).await?;
    metadata_store
        .get(&storage_key)
        .await?
        .with_context(|| format!("NATS session '{session_id}' was not found"))?;
    let log = NatsSessionLog::new(jetstream, storage_key);
    let raw = log
        .load_events_async()
        .await
        .with_context(|| format!("Failed to load NATS session '{session_id}'"))?;
    let entries = harnx_core::session_reconstruct::apply_log_mutations_nats(&raw)?;
    entries
        .into_iter()
        .map(|(_, entry)| serde_yaml::to_string(&entry).context("Failed to render session entry"))
        .collect::<Result<Vec<_>>>()
        .map(|documents| documents.join("---\n"))
}

/// Resolve an explicit frontend agent selector independently of the active agent.
pub async fn render_session_dump_for_agent_ref(
    config: &Config,
    agent_ref: &str,
    session_id: &str,
) -> Result<String> {
    use harnx_core::agent_ref::AgentRef;
    let (agent, cluster) = match AgentRef::parse(agent_ref) {
        AgentRef::Local(agent) => (agent, super::LOCAL_CLUSTER_KEY.into()),
        AgentRef::Remote { agent, cluster } => (agent, cluster),
    };
    render_session_dump_for_agent(config, &cluster, session_id, &agent).await
}
