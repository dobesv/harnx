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
    /// Whether the session has unread attention (attention_seq > read_seq or manual_unread).
    pub unread: bool,
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
    /// Namespaced client-visible state. The private execution-context
    /// namespace is always removed.
    pub extensions: BTreeMap<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_contexts: Option<Vec<RedactedRepositoryContext>>,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RedactedRepositoryContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// The repositories and branches tool calls in a session have worked in,
/// without the workspace paths and provenance the execution context keeps
/// private. A context that fails to decode counts as none.
pub fn repository_contexts(metadata: &SessionMetadata) -> Vec<RedactedRepositoryContext> {
    super::execution_contexts(metadata)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|context| {
            let repository = context.primary_repository().map(str::to_string);
            let branch = context.branch().map(str::to_string);
            (repository.is_some() || branch.is_some())
                .then_some(RedactedRepositoryContext { repository, branch })
        })
        .collect()
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
        let repository_contexts = repository_contexts(&record.metadata);
        let repository_contexts = (!repository_contexts.is_empty()).then_some(repository_contexts);
        let mut extensions = record.metadata.extensions;
        extensions.remove(harnx_core::execution_context::EXECUTION_CONTEXT_NAMESPACE);
        extensions.remove(super::TOOL_CONTEXT_NAMESPACE);
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
            extensions,
            repository_contexts,
            revision: record.revision,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionMetadataPatch {
    pub title: Option<SessionTitlePatch>,
    pub variables: Option<AgentVariables>,
    pub overrides: Option<SessionOverrides>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTitlePatch {
    pub value: Option<String>,
    #[serde(default)]
    pub manual: bool,
}

/// Apply a typed metadata patch in place.
///
/// Infallible by construction: each field is either replaced or left alone.
/// Shared by the metadata PATCH mutations and the initial canonical record a
/// `SessionInitializer`'s patch builds (`SessionMetadata::new`), so both paths
/// keep identical field semantics. Titles are stored trimmed, a blank title
/// clears the record (manual flag reset), and supplied variables or overrides
/// replace their initializer-derived values wholesale.
pub(in crate::nats_session_metadata) fn apply_typed_patch(
    metadata: &mut SessionMetadata,
    patch: &SessionMetadataPatch,
) {
    if let Some(title) = &patch.title {
        metadata.title.value = title
            .value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        metadata.title.manual = metadata.title.value.is_some() && title.manual;
        if metadata.title.value.is_none() {
            metadata.title.last_updated_tokens = 0;
        }
    }
    if let Some(variables) = &patch.variables {
        metadata.variables = variables.clone();
    }
    if let Some(overrides) = &patch.overrides {
        metadata.overrides = overrides.clone();
    }
}
