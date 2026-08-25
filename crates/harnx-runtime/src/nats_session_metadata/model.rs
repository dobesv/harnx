use super::{store::validate_extensions, SessionInitializer, SESSION_METADATA_SCHEMA_VERSION};
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use harnx_core::agent_config::AgentVariables;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionAgentSource {
    Named { name: String },
    Inline { instructions: String },
}

impl SessionAgentSource {
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Named { name } => Some(name),
            Self::Inline { .. } => None,
        }
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::Named { name } if name.trim().is_empty() => {
                bail!("named session agent must not be empty")
            }
            _ => Ok(()),
        }
    }

    fn identity_label(&self) -> String {
        match self {
            Self::Named { name } => format!("named agent '{name}'"),
            Self::Inline { .. } => "inline agent".to_string(),
        }
    }
}

/// Values explicitly pinned for a session and applied over a freshly loaded
/// named agent (or over the base worker configuration for an inline agent).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub model_fallbacks: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compress_threshold: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<isize>,
}

/// A field-scoped session override mutation.
///
/// Runtime `.set` and `.model` commands use this representation so concurrent
/// changes to different settings are merged by the metadata CAS loop instead
/// of replacing an override snapshot read before another writer committed.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionOverrideUpdate {
    Model(Option<String>),
    Temperature(Option<f64>),
    TopP(Option<f64>),
    UseTools(Option<Vec<String>>),
    ModelFallbacks(Vec<String>),
    CompressThreshold(Option<usize>),
    CompactionAgent(Option<String>),
    MaxOutputTokens(Option<isize>),
}

impl SessionOverrideUpdate {
    pub fn apply(&self, overrides: &mut SessionOverrides) {
        match self {
            Self::Model(value) => overrides.model = value.clone(),
            Self::Temperature(value) => overrides.temperature = *value,
            Self::TopP(value) => overrides.top_p = *value,
            Self::UseTools(value) => overrides.use_tools = value.clone(),
            Self::ModelFallbacks(value) => overrides.model_fallbacks = value.clone(),
            Self::CompressThreshold(value) => overrides.compress_threshold = *value,
            Self::CompactionAgent(value) => overrides.compaction_agent = value.clone(),
            Self::MaxOutputTokens(value) => overrides.max_output_tokens = *value,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionTitle {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub manual: bool,
    pub last_updated_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMetadata {
    pub schema_version: u32,
    pub session_id: String,
    pub created_at: DateTime<Utc>,
    pub agent: SessionAgentSource,
    #[serde(default)]
    pub variables: AgentVariables,
    #[serde(default)]
    pub overrides: SessionOverrides,
    #[serde(default)]
    pub title: SessionTitle,
    #[serde(default)]
    pub extensions: BTreeMap<String, Value>,
    /// Highest lease fence that has committed worker-owned metadata. This is
    /// deliberately omitted from the HTTP redacted view.
    #[serde(default)]
    pub(super) worker_fence_token: u64,
}

impl SessionMetadata {
    pub fn new(session_id: impl Into<String>, initializer: SessionInitializer) -> Self {
        Self {
            schema_version: SESSION_METADATA_SCHEMA_VERSION,
            session_id: session_id.into(),
            created_at: Utc::now(),
            agent: initializer.agent,
            variables: initializer.variables,
            overrides: initializer.overrides,
            title: SessionTitle::default(),
            extensions: BTreeMap::new(),
            worker_fence_token: 0,
        }
    }

    pub fn validate(&self, expected_session_id: &str) -> Result<()> {
        anyhow::ensure!(
            self.schema_version == SESSION_METADATA_SCHEMA_VERSION,
            "unsupported session metadata schema version {} for '{}'",
            self.schema_version,
            self.session_id
        );
        anyhow::ensure!(
            self.session_id == expected_session_id,
            "session metadata identity mismatch: expected '{expected_session_id}', found '{}'",
            self.session_id
        );
        self.agent.validate()?;
        validate_extensions(&self.extensions)
    }

    pub fn validate_initializer(&self, initializer: &SessionInitializer) -> Result<()> {
        anyhow::ensure!(
            self.agent == initializer.agent,
            "session agent identity mismatch for '{}': existing {}, requested {}",
            self.session_id,
            self.agent.identity_label(),
            initializer.agent.identity_label()
        );
        Ok(())
    }

    pub fn base_session(&self) -> harnx_core::session::Session {
        let mut session = harnx_core::session::Session {
            id: self.session_id.clone(),
            session_id: Some(self.session_id.clone()),
            agent_name: self.agent.name().map(str::to_string),
            agent_variables: self.variables.clone(),
            title: self.title.value.clone(),
            title_last_updated_tokens: if self.title.manual {
                usize::MAX
            } else {
                self.title.last_updated_tokens
            },
            model_id: self.overrides.model.clone().unwrap_or_default(),
            temperature: self.overrides.temperature,
            top_p: self.overrides.top_p,
            use_tools: self.overrides.use_tools.clone(),
            compress_threshold: self.overrides.compress_threshold,
            model_fallbacks: self.overrides.model_fallbacks.clone(),
            compaction_agent: self.overrides.compaction_agent.clone(),
            ..Default::default()
        };
        if let SessionAgentSource::Inline { instructions } = &self.agent {
            session.agent_instructions = instructions.clone();
        }
        session
    }
}
