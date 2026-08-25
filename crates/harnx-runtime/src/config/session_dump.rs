use super::Config;
use crate::nats_session_log::NatsSessionLog;
use anyhow::{bail, Context, Result};

/// Render the effective session log loaded from NATS as YAML documents.
pub async fn render_session_dump(
    config: &Config,
    cluster: &str,
    session_id: &str,
) -> Result<String> {
    render_session_dump_for_agent(config, cluster, session_id, None).await
}

pub async fn render_session_dump_for_agent(
    config: &Config,
    cluster: &str,
    session_id: &str,
    expected_agent: Option<&str>,
) -> Result<String> {
    let jetstream = config.nats_jetstream(cluster).await?;
    let metadata_store =
        crate::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1).await?;
    let metadata = metadata_store
        .get(session_id)
        .await?
        .with_context(|| format!("NATS session '{session_id}' was not found"))?
        .metadata;
    let log = NatsSessionLog::new(jetstream, session_id.to_string());
    let raw = log
        .load_events_async()
        .await
        .with_context(|| format!("Failed to load NATS session '{session_id}'"))?;
    let entries = harnx_core::session_reconstruct::apply_log_mutations_nats(&raw)?;
    if let Some(expected_agent) = expected_agent {
        let actual_agent = metadata.agent.name();
        if actual_agent != Some(expected_agent) {
            bail!(
                "NATS session '{session_id}' belongs to agent '{}', not '{expected_agent}'",
                actual_agent.unwrap_or("<unknown>")
            );
        }
    }
    entries
        .into_iter()
        .map(|(_, entry)| serde_yaml::to_string(&entry).context("Failed to render session entry"))
        .collect::<Result<Vec<_>>>()
        .map(|documents| documents.join("---\n"))
}
