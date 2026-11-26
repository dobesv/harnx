//! Host-only resume input. Durable response acceptance belongs to the caller.
use anyhow::{ensure, Result};
use harnx_core::interaction::{InteractionRequest, InteractionResponse, InteractionStep};
use harnx_engine::tool::{PreparedToolCall, ToolCallCheckpoint};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, future::Future};

/// An exact request and its runtime-accepted response, never model-authored input.
#[derive(Clone, Debug)]
pub struct AcceptedInteraction {
    pub request: InteractionRequest,
    pub response: InteractionResponse,
}

/// Rerun input for a suspended tool challenge: the latest challenge's
/// accepted responses (MRTR `inputResponses`) plus its opaque request state
/// (MRTR `requestState`), delivered beside the arguments, never inside them.
#[derive(Clone, Debug)]
pub struct ToolInputResume {
    pub responses: std::collections::BTreeMap<String, harnx_toolset::ToolInputResponse>,
    pub request_state: Value,
}

tokio::task_local! {
    static TOOL_INPUT: ToolInputResume;
}

/// The tool challenge request state (MRTR `requestState`) stored in a
/// restored call checkpoint, if the call ever challenged. Private checkpoint
/// JSON; only the T4.3 tool rerun path reads it.
pub(crate) fn checkpoint_tool_request_state(checkpoint: &ToolCallCheckpoint) -> Option<Value> {
    checkpoint
        .state
        .get("tool_request_state")
        .cloned()
        .filter(|state| !state.is_null())
}

/// Scope one tool challenge's rerun input to its owning call's leaf dispatch.
/// Allowlisted middleware `tools` calls run outside this scope, so
/// they never see another tool's answers.
pub async fn with_tool_input<F: Future>(input: ToolInputResume, future: F) -> F::Output {
    TOOL_INPUT.scope(input, future).await
}

/// The tool challenge rerun input for the current leaf dispatch, if any.
pub fn current_tool_input() -> Option<ToolInputResume> {
    TOOL_INPUT.try_with(Clone::clone).ok()
}

/// Restore one call from its last interaction boundary. This doesn't replay tools.
#[derive(Clone, Debug)]
pub struct MiddlewareResume {
    pub checkpoint: ToolCallCheckpoint,
    pub responses: Vec<AcceptedInteraction>,
    /// Rerun input for the call's latest tool challenge, when the call was
    /// suspended by a tool (producer `tool:<server>/<tool>`).
    pub tool_input: Option<ToolInputResume>,
}

tokio::task_local! {
    static RESUME: MiddlewareResume;
}

/// Supply trusted resume data while evaluating its owning call. Sibling calls with
/// other IDs ignore it. Invalid checkpoints for the owning call fail closed.
/// The caller must accept and persist responses before entering this scope.
pub async fn with_resume<F: Future>(resume: MiddlewareResume, future: F) -> F::Output {
    RESUME.scope(resume, future).await
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct FrameIdentity {
    pub middleware: String,
    pub step_path: Vec<InteractionStep>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FrameCheckpoint {
    pub middleware: String,
    pub step_path: Vec<InteractionStep>,
    pub stash: HashMap<String, Value>,
    pub retained: HashMap<String, Vec<String>>,
}

impl FrameCheckpoint {
    pub fn matches(&self, identity: &FrameIdentity) -> bool {
        self.middleware == identity.middleware && self.step_path == identity.step_path
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CheckpointState {
    pub version: u32,
    pub session: String,
    pub tool_round_seq: u64,
    pub tool_call_id: String,
    pub chain_version: String,
    pub frames: Vec<FrameCheckpoint>,
    /// The latest tool challenge's opaque request state (MRTR `requestState`),
    /// stored in the call's checkpoint and resent beside the responses on the
    /// rerun. Absent on calls that never challenged.
    #[serde(default)]
    pub tool_request_state: Option<Value>,
}

impl CheckpointState {
    pub fn new(session: String, prepared: &PreparedToolCall, chain_version: String) -> Self {
        Self {
            version: 1,
            session,
            tool_round_seq: prepared.context.evaluation.tool_round.unwrap_or_default(),
            tool_call_id: prepared.call.id.clone().unwrap_or_default(),
            chain_version,
            frames: vec![],
            tool_request_state: None,
        }
    }

    pub fn restore(&self) -> Result<(ToolCallCheckpoint, Vec<AcceptedInteraction>)> {
        let resume = RESUME.try_with(Clone::clone).ok();
        let Some(resume) = resume else {
            return Ok((ToolCallCheckpoint::default(), vec![]));
        };
        // Scope is per original call, not per tool name or model arguments.
        if resume.checkpoint.state["tool_call_id"] != self.tool_call_id {
            return Ok((ToolCallCheckpoint::default(), vec![]));
        }
        let state: Self = serde_json::from_value(resume.checkpoint.state.clone())?;
        ensure!(
            state.version == 1,
            "unsupported middleware checkpoint version"
        );
        ensure!(
            state.session == self.session
                && state.tool_round_seq == self.tool_round_seq
                && state.chain_version == self.chain_version,
            "middleware checkpoint identity mismatch"
        );
        Ok((resume.checkpoint, resume.responses))
    }
}
