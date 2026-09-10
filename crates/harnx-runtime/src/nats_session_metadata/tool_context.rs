use super::{MetadataRecord, SessionMetadata, SessionMetadataStore};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Reserved metadata namespace containing model-hidden tool routing state.
pub const TOOL_CONTEXT_NAMESPACE: &str = "dev.harnx.tool_context";
pub const TOOL_CONTEXT_VERSION: u32 = 1;

/// Private, durable state shared by trusted native tool servers.
///
/// Values are routing hints rather than secrets or authorization grants.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolContext {
    pub version: u32,
    pub values: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug)]
pub struct ToolContextEntry<'a> {
    pub session_id: &'a str,
    pub key: &'a str,
}

enum ToolContextChange {
    Replace(Value),
    Remove,
}

impl Default for ToolContext {
    fn default() -> Self {
        Self {
            version: TOOL_CONTEXT_VERSION,
            values: BTreeMap::new(),
        }
    }
}

impl ToolContext {
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.version == TOOL_CONTEXT_VERSION,
            "unsupported tool context version {}",
            self.version
        );
        for key in self.values.keys() {
            validate_tool_context_key(key)?;
        }
        Ok(())
    }
}

pub fn tool_context(metadata: &SessionMetadata) -> Result<ToolContext> {
    let Some(value) = metadata.extensions.get(TOOL_CONTEXT_NAMESPACE) else {
        return Ok(ToolContext::default());
    };
    let context: ToolContext = serde_json::from_value(value.clone())
        .context("decode dev.harnx.tool_context session extension")?;
    context.validate()?;
    Ok(context)
}

fn validate_tool_context_key(key: &str) -> Result<()> {
    anyhow::ensure!(
        !key.is_empty()
            && key.len() <= 128
            && key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "invalid tool context key '{key}'"
    );
    Ok(())
}

impl SessionMetadataStore {
    pub async fn get_tool_context(&self, session_id: &str) -> Result<Option<ToolContext>> {
        self.get(session_id)
            .await?
            .map(|record| tool_context(&record.metadata))
            .transpose()
    }

    pub async fn replace_tool_context_value(
        &self,
        entry: ToolContextEntry<'_>,
        value: Value,
    ) -> Result<MetadataRecord> {
        self.change_tool_context(entry, ToolContextChange::Replace(value))
            .await
    }

    pub async fn remove_tool_context_value(
        &self,
        entry: ToolContextEntry<'_>,
    ) -> Result<MetadataRecord> {
        self.change_tool_context(entry, ToolContextChange::Remove)
            .await
    }

    async fn change_tool_context(
        &self,
        entry: ToolContextEntry<'_>,
        change: ToolContextChange,
    ) -> Result<MetadataRecord> {
        validate_tool_context_key(entry.key)?;
        self.patch(entry.session_id, |metadata| {
            let mut context = tool_context(metadata)?;
            match &change {
                ToolContextChange::Replace(value) => {
                    context.values.insert(entry.key.to_string(), value.clone());
                }
                ToolContextChange::Remove => {
                    context.values.remove(entry.key);
                }
            }
            if context.is_empty() {
                metadata.extensions.remove(TOOL_CONTEXT_NAMESPACE);
            } else {
                context.validate()?;
                metadata.extensions.insert(
                    TOOL_CONTEXT_NAMESPACE.to_string(),
                    serde_json::to_value(context)?,
                );
            }
            Ok(())
        })
        .await
    }
}
