use super::{SessionActivity, SessionAgentSource, SessionMetadata, SessionOverrides, SessionTitle};
use chrono::{DateTime, Utc};
use harnx_core::agent_config::AgentVariables;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct MetadataRecord {
    pub metadata: SessionMetadata,
    pub revision: u64,
}

#[derive(Debug, Clone)]
pub struct ListedSession {
    pub metadata: SessionMetadata,
    pub metadata_revision: u64,
    pub activity: Option<SessionActivity>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedactedAgentSource {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedactedSessionMetadata {
    pub schema_version: u32,
    pub session_id: String,
    pub created_at: DateTime<Utc>,
    pub agent: RedactedAgentSource,
    pub title: SessionTitle,
    pub overrides: SessionOverrides,
    pub variables: BTreeMap<String, VariableStatus>,
    pub activity: Option<SessionActivity>,
    /// Namespaced client-visible state. Extension payloads are intentionally
    /// returned verbatim; callers must not use this field for secrets.
    pub extensions: BTreeMap<String, Value>,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct VariableStatus {
    pub set: bool,
}

impl RedactedSessionMetadata {
    pub fn new(record: MetadataRecord, activity: Option<SessionActivity>) -> Self {
        let agent = match &record.metadata.agent {
            SessionAgentSource::Named { name } => RedactedAgentSource {
                kind: "named",
                name: Some(name.clone()),
            },
            SessionAgentSource::Inline { .. } => RedactedAgentSource {
                kind: "inline",
                name: None,
            },
        };
        Self {
            schema_version: record.metadata.schema_version,
            session_id: record.metadata.session_id,
            created_at: record.metadata.created_at,
            agent,
            title: record.metadata.title,
            overrides: record.metadata.overrides,
            variables: record
                .metadata
                .variables
                .into_keys()
                .map(|name| (name, VariableStatus { set: true }))
                .collect(),
            activity,
            extensions: record.metadata.extensions,
            revision: record.revision,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionMetadataPatch {
    pub title: Option<SessionTitlePatch>,
    pub variables: Option<AgentVariables>,
    pub overrides: Option<SessionOverrides>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTitlePatch {
    pub value: Option<String>,
    #[serde(default)]
    pub manual: bool,
}
