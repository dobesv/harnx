//! The `harnx_read_session_meta` and `harnx_write_session_meta` built-in
//! tools: an agent reads its own session's metadata and records what the
//! session is working on. They are declared only for agents whose `use_tools`
//! names them, and they reach the metadata through the active session's
//! persistence sink, so a worker writes under its lease fence.

use crate::config::GlobalConfig;
use crate::nats_session_metadata::{
    repository_contexts, session_properties, Inheritance, SessionMetadata, SessionPropertiesUpdate,
    PROPERTY_DEFINITIONS,
};
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use harnx_core::abort::AbortSignal;
use harnx_core::event::ToolKind;
use harnx_core::tool::{JsonSchema, ToolDeclaration, ToolError, ToolProvider, ToolProviderOutput};
use serde_json::{json, Value};
use std::future::Future;
use std::time::Duration;

pub const READ_TOOL_NAME: &str = "harnx_read_session_meta";
pub const WRITE_TOOL_NAME: &str = "harnx_write_session_meta";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn declaration(name: &str, description: String, parameters: Value) -> ToolDeclaration {
    let reads = name == READ_TOOL_NAME;
    ToolDeclaration {
        name: name.to_string(),
        description,
        parameters: serde_json::from_value::<JsonSchema>(parameters)
            .expect("session metadata tool schema is valid"),
        mcp_tool_name: None,
        mcp_server_name: None,
        call_template: None,
        result_template: None,
        idempotent_hint: Some(true),
        read_only_hint: Some(reads),
        kind: Some(if reads {
            ToolKind::Read
        } else {
            ToolKind::Edit
        }),
    }
}

fn joined_names(
    filter: impl Fn(&crate::nats_session_metadata::PropertyDefinition) -> bool,
) -> String {
    PROPERTY_DEFINITIONS
        .iter()
        .filter(|definition| filter(definition))
        .map(|definition| definition.name)
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn read_tool_declaration() -> ToolDeclaration {
    let description = format!(
        "Read this session's metadata: its session ID, agent, title and creation time; its \
properties, each a value plus an inherit flag saying whether sub-agent sessions started from \
this session copy it; and observed_repositories, the repositories and branches this session's \
tool calls have worked in. Well-known properties are {}. Returns a JSON object.",
        joined_names(|_| true)
    );
    declaration(
        READ_TOOL_NAME,
        description,
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
    )
}

pub fn write_tool_declaration() -> ToolDeclaration {
    let properties = PROPERTY_DEFINITIONS
        .iter()
        .map(|definition| {
            let read_only = if definition.read_only {
                " (read-only)"
            } else {
                ""
            };
            format!(
                "- {}: {}{read_only}",
                definition.name, definition.description
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let description = format!(
        "Record what this session is working on, so people and tools can find it. Each property \
holds a value and an inherit flag; sub-agent sessions started from this session copy the \
properties marked inherit. Well-known properties:\n{properties}\nAny other name that starts with \
a lowercase letter and uses only lowercase letters, digits, '_', '-' and '.' is a custom property \
holding text. `clear` runs first, then `set`, then remove_labels, then add_labels. Setting a \
property only records it: it doesn't check out a branch, change directory or contact GitHub. \
Returns the session's metadata after the change."
    );
    declaration(WRITE_TOOL_NAME, description, write_tool_schema())
}

fn string_list(description: &str) -> Value {
    json!({"type": "array", "items": {"type": "string"}, "description": description})
}

fn write_tool_schema() -> Value {
    let inherit = format!(
        "Whether sub-agent sessions started from this session copy the property. Omit to keep \
the current setting; a new property is then inherited only if it is one of {}. {} is never \
inherited.",
        joined_names(|definition| {
            definition.inheritance == Inheritance::ByDefault && !definition.read_only
        }),
        joined_names(|definition| definition.inheritance == Inheritance::Never),
    );
    json!({
        "type": "object",
        "properties": {
            "set": {
                "type": "array",
                "description": "Properties to set. A blank value, 0 for a number, or an empty list removes the property instead.",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "A well-known or custom property name."},
                        "value": {
                            "description": "A number for github_issue and github_pull_request, a list of strings for labels, and text otherwise.",
                            "anyOf": [
                                {"type": "string"},
                                {"type": "integer"},
                                {"type": "array", "items": {"type": "string"}}
                            ]
                        },
                        "inherit": {"type": "boolean", "description": inherit}
                    },
                    "required": ["name", "value"],
                    "additionalProperties": false
                }
            },
            "clear": string_list("Names of properties to remove."),
            "add_labels": string_list("Labels to add."),
            "remove_labels": string_list("Labels to remove.")
        },
        "additionalProperties": false
    })
}

/// What the tools return: the session's identity and properties, and the
/// repositories its tool calls have worked in, without private paths.
pub fn session_meta_view(metadata: &SessionMetadata) -> anyhow::Result<Value> {
    Ok(json!({
        "session_id": metadata.session_id,
        "agent": metadata.agent.name(),
        "title": metadata.title.value,
        "created_at": metadata.created_at,
        "properties": session_properties(metadata)?,
        "observed_repositories": repository_contexts(metadata),
    }))
}

/// `ToolProvider` for the session metadata tools, acting on whichever session
/// is active in the captured config when a call arrives.
pub struct SessionMetaProvider {
    config: GlobalConfig,
}

impl SessionMetaProvider {
    pub fn new(config: GlobalConfig) -> Self {
        Self { config }
    }
}

fn recoverable(error: anyhow::Error) -> ToolError {
    ToolError::Recoverable(error)
}

/// Run one metadata round trip, giving up when the turn is aborted or the
/// broker doesn't answer in time.
async fn bounded<T>(
    operation: impl Future<Output = anyhow::Result<T>>,
    abort: &AbortSignal,
) -> Result<T, ToolError> {
    tokio::select! {
        _ = harnx_core::abort::wait_abort_signal(abort) => {
            Err(recoverable(anyhow!("session metadata request aborted")))
        }
        result = tokio::time::timeout(REQUEST_TIMEOUT, operation) => result
            .map_err(|_| recoverable(anyhow!("session metadata request timed out")))?
            .map_err(recoverable),
    }
}

#[async_trait]
impl ToolProvider for SessionMetaProvider {
    fn name(&self) -> &str {
        "session_meta"
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        matches!(tool_name, READ_TOOL_NAME | WRITE_TOOL_NAME)
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
        abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        let no_metadata = || recoverable(anyhow!("this session has no canonical metadata"));
        // Taken under a short guard: the sink's methods are NATS round trips,
        // and the config lock must never be held across one.
        let sink = crate::config::session::active_session_sink(&self.config, None)
            .ok_or_else(no_metadata)?;
        let metadata = if tool_name == WRITE_TOOL_NAME {
            let update: SessionPropertiesUpdate = serde_json::from_value(arguments)
                .context("invalid harnx_write_session_meta arguments")
                .map_err(recoverable)?;
            bounded(sink.persist_session_properties(&update), abort).await?
        } else {
            bounded(sink.load_metadata(), abort).await?
        }
        .ok_or_else(no_metadata)?;
        let view = session_meta_view(&metadata).map_err(recoverable)?;
        Ok(json!({ "content": [{ "type": "text", "text": view.to_string() }] }).into())
    }
}

#[cfg(test)]
#[path = "session_meta_tool_tests.rs"]
mod tests;
