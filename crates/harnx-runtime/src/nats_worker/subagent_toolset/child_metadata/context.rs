//! The trusted-context and saved-checkpoint-scope validation a patch-bearing
//! call must pass BEFORE its metadata gate can pin a guard or mutate
//! anything (reviews F02/F03): a direct/MCP client can carry a real durable
//! journal while its inherited run context is missing or malformed, and a
//! same-broker route change can leave a saved checkpoint's scope stale while
//! the journaled request and child record stay valid.

use super::SubagentToolset;
use crate::nats_session_metadata::RunLimitsRecord;
use harnx_toolset::{ToolInvocationContext, ToolInvokeError};
use serde_json::{json, Value};

/// The inherited run context a synchronous prompt requires, validated once
/// up front so the metadata gate can never run ahead of context rejection
/// (review F02).
pub(in crate::nats_worker::subagent_toolset) struct InheritedPrompt {
    pub(in crate::nats_worker::subagent_toolset) parent: RunLimitsRecord,
    pub(in crate::nats_worker::subagent_toolset) admitted_at: chrono::DateTime<chrono::Utc>,
}

impl InheritedPrompt {
    pub(in crate::nats_worker::subagent_toolset) fn validate(
        context: &ToolInvocationContext,
    ) -> Result<Self, ToolInvokeError> {
        let lineage = context.run_context.as_ref().ok_or_else(|| {
            ToolInvokeError::Fatal("sub-agent execution requires inherited run context".into())
        })?;
        let parent: RunLimitsRecord =
            serde_json::from_value(lineage.snapshot.clone()).map_err(|error| {
                ToolInvokeError::Fatal(format!("invalid inherited run context: {error}"))
            })?;
        let admitted_at = chrono::DateTime::from_timestamp_millis(
            i64::try_from(lineage.started_at_ms)
                .map_err(|error| ToolInvokeError::Fatal(error.to_string()))?,
        )
        .ok_or_else(|| ToolInvokeError::Fatal("invalid original invocation time".into()))?;
        Ok(Self {
            parent,
            admitted_at,
        })
    }
}

/// Why a saved invocation checkpoint does not match this worker's route.
pub(in crate::nats_worker::subagent_toolset) enum ScopeMismatch {
    /// The saved checkpoint names no usable session id.
    MissingId,
    /// A scope field names another route, agent or identity.
    Scope,
}

/// The core scope identity a saved invocation checkpoint must carry for this
/// worker's route: the session id it names, the storage key this worker
/// derives for it, and the current cluster label. `extra` adds fields a
/// checkpoint shape requires on top of the core (the async dispatch
/// checkpoint pins its prompt id and parent storage key too). Returns the
/// session id when every field matches (review F03).
pub(in crate::nats_worker::subagent_toolset) fn saved_checkpoint_scope(
    toolset: &SubagentToolset,
    saved: &Value,
    extra: &[(&str, &str)],
) -> Result<String, ScopeMismatch> {
    let id = saved["session_id"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .ok_or(ScopeMismatch::MissingId)?;
    let mut expected = json!({
        "session_id": id,
        "storage_key": harnx_core::session_identity::session_key(Some(&toolset.agent), id),
        "cluster": toolset.route.cluster(),
    });
    for (key, value) in extra {
        expected[*key] = json!(*value);
    }
    let matches = expected.as_object().is_some_and(|expected| {
        expected
            .iter()
            .all(|(key, value)| saved.get(key) == Some(value))
    });
    if matches {
        Ok(id.to_string())
    } else {
        Err(ScopeMismatch::Scope)
    }
}

/// The saved invocation checkpoint a patch-bearing synchronous replay
/// targets must match this worker's route scope before the metadata gate
/// can pin a guard or mutate. Same fields `checkpointed_session` checkpoints;
/// metadata-free and admitted calls keep their existing behavior.
pub(in crate::nats_worker::subagent_toolset) fn validate_sync_saved_scope(
    toolset: &SubagentToolset,
    saved: &Value,
) -> Result<(), ToolInvokeError> {
    saved_checkpoint_scope(toolset, saved, &[])
        .map(|_| ())
        .map_err(|mismatch| {
            ToolInvokeError::Fatal(
                match mismatch {
                    ScopeMismatch::MissingId => "invalid sub-agent checkpoint session_id",
                    ScopeMismatch::Scope => "sub-agent checkpoint scope mismatch",
                }
                .into(),
            )
        })
}
