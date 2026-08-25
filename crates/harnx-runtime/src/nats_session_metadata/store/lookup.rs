use super::*;
use anyhow::Result;

impl SessionMetadataStore {
    pub async fn get_for_agent(
        &self,
        session_id: &str,
        agent: &str,
    ) -> Result<Option<MetadataRecord>> {
        Ok(self
            .get(session_id)
            .await?
            .filter(|record| metadata_belongs_to_agent(&record.metadata, agent)))
    }
}

pub(in crate::nats_session_metadata) fn metadata_belongs_to_agent(
    metadata: &SessionMetadata,
    agent: &str,
) -> bool {
    matches!(&metadata.agent, SessionAgentSource::Named { name } if name == agent)
}
