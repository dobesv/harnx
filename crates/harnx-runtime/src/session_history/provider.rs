//! The `ToolProvider` half of the session-history tool: its declaration,
//! argument parsing and the provider that resolves the active session's
//! NATS log at call time. Pure query core lives in the parent module.

use super::{query_entries_with_jaq, HistoryQuery, TOOL_NAME};
use crate::{config::GlobalConfig, tool::session_json_output, ScriptShape};
use async_trait::async_trait;
use harnx_core::abort::AbortSignal;
use harnx_core::session::SessionLogEntry;
use harnx_core::tool::{
    JsonSchema, NoopToolProgress, ToolCallOrigin, ToolDeclaration, ToolError, ToolProgress,
    ToolProvider, ToolProviderOutput,
};
use serde_json::{json, Value};
use std::sync::Arc;

// qual:api — re-exported as crate::session_history::tool_declaration for callers building the tool's declaration (tests and the tool wiring).
/// Build the tool declaration (name, description, JSON-schema parameters).
pub fn tool_declaration() -> ToolDeclaration {
    let schema = json!({
        "type": "object",
        "properties": {
            "index_min": {"type": "integer", "description": "Minimum log entry seq (inclusive)."},
            "index_max": {"type": "integer", "description": "Maximum log entry seq (inclusive)."},
            "type": {"type": "string", "description": "Filter by entry type: message, tool_calls, tool_results, sub_agent_started, handoff_committed, hitl_approval_requested, hitl_approval_decision, compress, data_urls, clear, cancel, error, turn_end, admission_closed, edit_entries, rewind, unknown."},
            "tool_name": {"type": "string", "description": "Keep only entries referencing this tool name."},
            "text_regex": {"type": "string", "description": "Keep entries whose rendered text matches this regular expression."},
            "limit": {"type": "integer", "description": "Maximum number of rows to return."},
            "jaq": {"type": "string", "description": "Optional jq/jaq expression applied to the array of matched rows for arbitrary filtering or projection. Invalid expressions return an error."}
        }
    });
    ToolDeclaration {
        name: TOOL_NAME.to_string(),
        description: "Search this session's own history log, including detail dropped by compaction. \
Filter by entry index range, type, tool name, or a text regex; optionally refine with a jaq expression. \
Returns a JSON array of matching log entries (seq, type, text, tool names)."
            .to_string(),
        parameters: serde_json::from_value::<JsonSchema>(schema)
            .expect("session-history tool schema is valid"),
        mcp_tool_name: None,
        mcp_server_name: None,
        call_template: None,
        result_template: None,
        idempotent_hint: None,
        read_only_hint: None,
        kind: None,
    }
}

/// Parse the tool-call arguments JSON into a `HistoryQuery`.
pub(super) fn parse_query(arguments: &Value) -> HistoryQuery {
    let s = |k: &str| {
        arguments
            .get(k)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let u = |k: &str| {
        arguments
            .get(k)
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
    };
    HistoryQuery {
        index_min: u("index_min"),
        index_max: u("index_max"),
        entry_type: s("type"),
        tool_name: s("tool_name"),
        text_regex: s("text_regex"),
        limit: u("limit"),
        jaq: s("jaq"),
    }
}

/// `ToolProvider` for the session-history tool. Resolves the active session's
/// NATS log from the captured config at call time (own session only).
pub struct SessionHistoryProvider {
    config: GlobalConfig,
}

impl SessionHistoryProvider {
    pub fn new(config: GlobalConfig) -> Self {
        Self { config }
    }

    pub(crate) fn script_shape() -> ScriptShape {
        // A jaq projection can return any JSON value, not just an array.
        ScriptShape {
            output: Some(harnx_toolset::ScriptOutput {
                summary: Some(
                    "JSON array of matching entries (seq, type, optional text, tool_names, role), \
                     or the JSON value produced by --jaq."
                        .to_string(),
                ),
                ..Default::default()
            }),
            output_schema: None,
        }
    }

    async fn load_entries(
        &self,
        abort: &AbortSignal,
    ) -> Result<Vec<(u64, SessionLogEntry)>, ToolError> {
        let (config, name, cluster) = {
            let guard = self.config.read();
            let session = guard
                .session
                .as_ref()
                .ok_or_else(|| ToolError::Recoverable(anyhow::anyhow!("no active session")))?;
            let cluster = guard
                .remote_agent
                .as_ref()
                .map(|(_, cluster)| cluster.clone())
                .unwrap_or_else(|| guard.default_cluster_key().to_string());
            (guard.clone(), session.storage_key(), cluster)
        };
        let operation = async {
            let jetstream = config.nats_jetstream(&cluster).await?;
            crate::nats_session_log::NatsSessionLog::new(jetstream, name)
                .load_events_async()
                .await
        };
        tokio::select! {
            _ = harnx_core::abort::wait_abort_signal(abort) => {
                Err(ToolError::Recoverable(anyhow::anyhow!("session history request aborted")))
            }
            result = tokio::time::timeout(std::time::Duration::from_secs(30), operation) => {
                result
                    .map_err(|_| ToolError::Recoverable(anyhow::anyhow!("session history request timed out")))?
                    .map_err(ToolError::Recoverable)
            }
        }
    }
}

#[async_trait]
impl ToolProvider for SessionHistoryProvider {
    fn name(&self) -> &str {
        "session_history"
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        tool_name == TOOL_NAME
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
        abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        self.call_tool_with_progress(
            tool_name,
            arguments,
            ToolCallOrigin::default(),
            abort,
            Arc::new(NoopToolProgress),
        )
        .await
    }

    async fn call_tool_with_progress(
        &self,
        _tool_name: &str,
        arguments: Value,
        origin: ToolCallOrigin<'_>,
        abort: &AbortSignal,
        _progress: Arc<dyn ToolProgress>,
    ) -> Result<ToolProviderOutput, ToolError> {
        let entries: Vec<_> = self
            .load_entries(abort)
            .await?
            .into_iter()
            .map(|(seq, entry)| (seq as usize, entry))
            .collect();
        let rows = query_entries_with_jaq(&entries, &parse_query(&arguments))
            .map_err(ToolError::Recoverable)?;
        Ok(session_json_output(rows, origin.structured_result))
    }
}

#[cfg(test)]
mod tests;
