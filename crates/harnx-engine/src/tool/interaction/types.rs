use crate::tool::{MaterializedToolResult, ToolCall, ToolResult};
use anyhow::Result;
use harnx_core::interaction::InteractionRequest;
use serde_json::Value;
use std::{collections::HashMap, future::Future, pin::Pin};

/// Host-only resume data. `state` contains frame stashes/step metadata; snapshots
/// retain private execution observations and media. Never serialize snapshots via
/// ToolResult's public serde representation (it skips execution_context).
#[derive(Debug, Clone, Default)]
pub struct ToolCallCheckpoint {
    pub state: Value,
    pub snapshots: HashMap<String, MaterializedToolResult>,
}

#[derive(Debug, Clone)]
pub struct PendingToolInteraction {
    pub call: ToolCall,
    pub interaction: InteractionRequest,
    pub checkpoint: ToolCallCheckpoint,
}

/// Original call order within each list. All siblings have settled before this
/// reaches the interaction callback. Runtime must durably classify failed calls
/// before exposing any request when handling a ToolRoundFailure.
#[derive(Debug, Clone)]
pub struct ToolRoundSuspension {
    pub completed: Vec<ToolResult>,
    pub pending: Vec<PendingToolInteraction>,
}

#[derive(Debug)]
pub enum ToolRoundOutcome {
    Completed(Vec<ToolResult>),
    InputRequired(ToolRoundSuspension),
}

/// Finalize/render/checkpoint one effective result before terminal publication.
/// Failure prevents publication and is fatal, but doesn't discard other siblings.
pub type ToolCallCompletedFn =
    dyn Fn(ToolResult) -> Pin<Box<dyn Future<Output = Result<ToolResult>> + Send>> + Send + Sync;

/// Called once per suspended round, after every completion callback finishes.
/// Runtime persists checkpoints, requests and the manifest before showing input.
pub type ToolInteractionFn =
    dyn Fn(ToolRoundSuspension) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync;

/// Context on a fatal round error; the original error remains downcastable.
#[derive(Debug)]
pub struct ToolRoundFailure {
    pub completed: Vec<ToolResult>,
    pub pending: Vec<PendingToolInteraction>,
    // Keep legacy error displays stable while attaching recoverable sibling state.
    pub(super) message: String,
}

/// Compatibility boundary for callers that still require a plain result vector.
/// New durable callers consume ToolRoundOutcome instead of treating this as failure.
#[derive(Debug)]
pub struct ToolInputRequiredError(pub ToolRoundSuspension);

macro_rules! interaction_error {
    ($name:ty, $error:ident => $message:expr) => {
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let $error = self;
                f.write_str($message)
            }
        }
        impl std::error::Error for $name {}
    };
}
interaction_error!(ToolRoundFailure, error => &error.message);
interaction_error!(ToolInputRequiredError, _error => "tool interaction input required");
